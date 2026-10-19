//! The C12/D27 owner retains the project lease across durable lifecycle steps.
//! Adapters supply intent artifacts; they cannot acquire an intent writer.

use crate::apply_intent::{
    ApplyContextV1, ApplyEvidenceV1, ApplyGenerationV1, ApplyIntentDescriptorV1, ApplyIntentError,
    ApplyIntentStoreV1, ApplyInventoryV1, CanonicalApplyEvidence, InspectedApplyIntentV1,
};
use crate::management_effect::*;
use crate::project_mutation::{
    ProjectMutationError, ProjectMutationLeaseV1, ProjectMutationOwnerV1,
};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ApplyOwnerError {
    #[error(transparent)]
    Intent(#[from] ApplyIntentError),
    #[error(transparent)]
    Management(#[from] ManagementEffectStoreError),
    #[error(transparent)]
    Lease(#[from] ProjectMutationError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Participation(#[from] crate::sync::participation::C12MutationErrorV1),
    #[error(transparent)]
    StrictParticipation(#[from] crate::sync::participation::ParticipationErrorV2),
    #[error(transparent)]
    Admission(#[from] crate::apply_intent::ApplyAdmissionErrorV1),
    #[error(transparent)]
    Desktop(#[from] crate::desktop_store::StoreError),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[error(transparent)]
    DesktopRecovery(#[from] crate::desktop_store::D27RecoveryOpenError),
    #[error("apply capacity unavailable before source dispatch")]
    ResourceUnavailableBeforeEffect,
    #[error("apply intent and management event do not match")]
    EvidenceMismatch,
}

#[derive(Debug)]
pub struct ApplyOwnerV1 {
    project_root: PathBuf,
    state_dir: PathBuf,
    lease: ProjectMutationLeaseV1,
}

/// The adapter retains the agreed Git revision and supplies the exact body it
/// intends to publish. Filesystem identity, capacity and marker hashes are
/// derived by the owner rather than accepted as adapter assertions.
#[derive(Clone, Debug)]
pub struct C12ApplyAdmissionV1 {
    pub management_operation_id: ManagementOperationIdV1,
    pub agreed_revision: crate::apply_intent::RevisionV1,
    pub staged_revision: crate::apply_intent::RevisionV1,
    pub previous_applied_revision: Option<crate::apply_intent::RevisionV1>,
    pub target_participation: Vec<u8>,
}

/// Trusted request coordinates; the owner derives filesystem and durable
/// bindings again from the writer, held snapshot and recovery descriptors.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct D27ApplyAdmissionV1 {
    pub target: polaris_desktop_protocol::request::RunTarget,
    pub approval_id: polaris_desktop_protocol::ids::ApprovalId,
    pub result_id: polaris_desktop_protocol::ids::ResultId,
    pub payload_hash: String,
    pub guard: crate::desktop_store::SourceApplyGuard,
    pub proof: crate::desktop_store::SourceApplyIdentityProof,
}

/// One consumed operation retains the shared lease across the source worker
/// and desktop writer. Adapters cannot construct or clone dispatch authority.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct D27ApplyOperationV1 {
    owner: Option<ApplyOwnerV1>,
    desktop_root_directory: std::fs::File,
    desktop_result_remaining_bytes: u64,
    coordinates: (
        polaris_desktop_protocol::ids::ProjectId,
        polaris_desktop_protocol::ids::SessionId,
    ),
    precommit: crate::desktop_store::SourceApplyPrecommitV1,
    descriptor: ApplyIntentDescriptorV1,
    store: ManagementEffectStoreV1,
    prepared: Option<PreparedManagementEffectV1>,
    started: Option<StartedManagementEffectV1>,
    reserved: Option<crate::workspace_apply::ReservedRecoveryV2>,
    reported: bool,
    completed: bool,
}

#[derive(Debug)]
pub enum ApplyOwnerRecoveryV1 {
    Idle,
    Archived(ManagementEffectHashV1),
    OutcomeUnknown(VerifiedManagementEffectEventV1),
}

#[cfg(any(test, feature = "execution-test-support"))]
thread_local! {
    static OWNER_STOP: std::cell::Cell<Option<crate::apply_intent::ApplyPhaseV1>> = const { std::cell::Cell::new(None) };
    static OWNER_AVAILABLE_LIMIT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "execution-test-support"))]
fn owner_stop_after(phase: crate::apply_intent::ApplyPhaseV1) -> Result<(), ApplyOwnerError> {
    if OWNER_STOP.with(|slot| slot.get() == Some(phase)) {
        OWNER_STOP.with(|slot| slot.set(None));
        return Err(std::io::Error::other("injected owner stop after durable phase").into());
    }
    Ok(())
}

impl ApplyOwnerV1 {
    #[cfg(any(test, feature = "execution-test-support"))]
    #[doc(hidden)]
    pub fn test_available_limit(bytes: Option<u64>) {
        OWNER_AVAILABLE_LIMIT.with(|slot| slot.set(bytes));
    }

    fn require_live_capacity(
        &self,
        plan: &crate::apply_intent::CapacityPlanV1,
        recovery: &std::fs::File,
        desktop: Option<&std::fs::File>,
        state_remaining: u64,
        recovery_remaining: u64,
        desktop_remaining: Option<u64>,
    ) -> Result<(), ApplyOwnerError> {
        use crate::apply_intent::*;
        let state = crate::project_mutation::open_directory_nofollow(&self.state_dir)?;
        let fresh = CapacityRootsV1::from_pinned_files(&state, recovery, desktop)?;
        #[cfg(any(test, feature = "execution-test-support"))]
        let fresh = {
            let mut fresh = fresh;
            if let Some(limit) = OWNER_AVAILABLE_LIMIT.with(|slot| slot.get()) {
                fresh.state.available_bytes = fresh.state.available_bytes.min(limit);
                fresh.recovery.available_bytes = fresh.recovery.available_bytes.min(limit);
                if let Some(root) = &mut fresh.desktop {
                    root.available_bytes = root.available_bytes.min(limit);
                }
            }
            fresh
        };
        if plan
            .live_capacity_groups(
                &fresh,
                state_remaining,
                recovery_remaining,
                desktop_remaining,
            )?
            .iter()
            .any(|group| group.shortage_bytes != 0)
        {
            return Err(ApplyIntentError::Capacity("live device group").into());
        }
        Ok(())
    }

    fn remaining_state_capacity(
        intent: &InspectedApplyIntentV1,
        generic: &ManagementEffectStateV1,
        seal_needed: bool,
    ) -> Result<u64, ApplyOwnerError> {
        use crate::apply_intent::*;
        let plan = &intent.descriptor().capacity_plan;
        let unit = plan.state_allocation_unit;
        let future = if intent.latest().phase.is_terminal() {
            0
        } else {
            MAX_INTENT_GENERATIONS
                .checked_sub(intent.generations.len() as u64)
                .ok_or(ApplyIntentError::Overflow)?
        };
        let lengths = management_append_lengths(&Self::management_request(intent.descriptor())?)?;
        let start = match generic {
            ManagementEffectStateV1::Prepared => 1,
            ManagementEffectStateV1::EffectStarted => 2,
            ManagementEffectStateV1::OutcomeUnknown { .. } => 3,
            ManagementEffectStateV1::Completed { .. }
            | ManagementEffectStateV1::Denied { .. }
            | ManagementEffectStateV1::ReconciledAbandon { .. } => lengths.len(),
        };
        let append = lengths[start..]
            .iter()
            .try_fold(0u64, |a, b| a.checked_add(*b))
            .ok_or(ApplyIntentError::Overflow)?;
        let generations = allocated_bytes(MAX_INTENT_GENERATION_BYTES, unit)?
            .checked_mul(future + u64::from(future != 0))
            .ok_or(ApplyIntentError::Overflow)?;
        let archive = if intent.seal.is_some() {
            0
        } else {
            allocated_bytes(plan.archive_seal_bytes, unit)?
                .checked_mul(2)
                .ok_or(ApplyIntentError::Overflow)?
        };
        [
            generations,
            archive,
            allocated_bytes(append, unit)?,
            if seal_needed {
                plan.management_generation_seal_required_bytes
            } else {
                0
            },
            if future != 0 || archive != 0 {
                plan.state_directory_overhead_bytes
            } else {
                0
            },
        ]
        .into_iter()
        .try_fold(0u64, |a, b| a.checked_add(b))
        .ok_or_else(|| ApplyIntentError::Overflow.into())
    }
    /// Deterministic durable-boundary fault for the existing test feature.
    #[cfg(any(test, feature = "execution-test-support"))]
    #[doc(hidden)]
    pub fn test_stop_after(phase: Option<crate::apply_intent::ApplyPhaseV1>) {
        OWNER_STOP.with(|slot| slot.set(phase));
    }
    /// Pin the C12 parent through the owned sync directory. A caller must not
    /// create it by following an unverified pathname before intent admission.
    pub fn c12_recovery_parent(
        &self,
        create: bool,
    ) -> Result<crate::workspace_apply::RecoveryParent, ApplyOwnerError> {
        use crate::project_mutation::open_directory_nofollow;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::MetadataExt;
        if create && self.active()?.is_some() {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let sync_path = self.state_dir.join("sync");
        let sync = open_directory_nofollow(&sync_path)?;
        let source = open_directory_nofollow(&self.project_root)?;
        let sync_meta = sync.metadata()?;
        if sync_meta.dev() != source.metadata()?.dev()
            || sync_meta.uid() != unsafe { libc::geteuid() }
            || sync_meta.mode() & 0o777 != 0o700
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let name = c"recovery";
        if create {
            if unsafe { libc::mkdirat(sync.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
                sync.sync_all()?;
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        let raw = unsafe {
            libc::openat(
                sync.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let directory = unsafe { std::fs::File::from_raw_fd(raw) };
        let meta = directory.metadata()?;
        let path = sync_path.join("recovery");
        let fresh_sync = open_directory_nofollow(&sync_path)?.metadata()?;
        let fresh_child = open_directory_nofollow(&path)?.metadata()?;
        if (fresh_sync.dev(), fresh_sync.ino()) != (sync_meta.dev(), sync_meta.ino())
            || (fresh_child.dev(), fresh_child.ino()) != (meta.dev(), meta.ino())
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        crate::workspace_apply::RecoveryParent::pin(&directory, (meta.dev(), meta.ino()), &path)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)
    }

    /// Normal sync commands retain this lease across staging and participation
    /// publication, so another apply cannot change the agreed candidate.
    pub fn ensure_sync_idle(&self) -> Result<(), ApplyOwnerError> {
        crate::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
            &self.project_root,
            &self.state_dir,
            &self.lease,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)
    }

    pub fn join_participation(
        &self,
        _share: crate::sync::share::ShareLink,
        _manifest: crate::sync::share::ShareManifest,
        _now_unix_seconds: u64,
    ) -> Result<crate::sync::participation::Participation, ApplyOwnerError> {
        // C08 join/rebind writes belong exclusively to CredentialRebindOwnerV1.
        // This legacy apply adapter has neither its binding nor intent proof.
        Err(ApplyOwnerError::EvidenceMismatch)
    }

    pub fn write_participation(
        &self,
        target: &crate::sync::participation::Participation,
    ) -> Result<(), ApplyOwnerError> {
        use crate::sync::participation;
        self.ensure_sync_idle()?;
        let inventory = participation::C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
        Ok(participation::write_under_owner(&inventory, target)?)
    }

    /// Retain the current participation schema and credential binding while
    /// constructing a C12 target under the same project lease.
    pub fn participation_target_bytes(
        &self,
        target: &crate::sync::participation::Participation,
    ) -> Result<Vec<u8>, ApplyOwnerError> {
        use crate::sync::participation;
        self.ensure_sync_idle()?;
        let inventory = participation::C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
        let current = inventory
            .current_body()?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        Ok(participation::updated_participation_bytes_v2(
            &current, target,
        )?)
    }

    pub fn leave_participation(&self) -> Result<bool, ApplyOwnerError> {
        use crate::sync::participation;
        self.ensure_sync_idle()?;
        let inventory = participation::C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
        Ok(participation::leave_under_owner(&inventory)?)
    }

    /// Shared C08/C11/C13 entry point: preserve the lease and record the
    /// local effect before dispatch. Failures remain unknown, never replayed.
    fn sync_change<T>(
        &self,
        inventory: &str,
        request: &[u8],
        accepted: bool,
        change: impl FnOnce() -> Result<T, ApplyOwnerError>,
    ) -> Result<Option<T>, ApplyOwnerError> {
        self.ensure_sync_idle()?;
        let effects = ManagementEffectStoreV1::open_at_state_dir(
            &self.project_root,
            &self.state_dir,
            &self.lease,
        )?;
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::random()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            inventory: ManagementInventoryIdV1::parse(inventory)
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            scope: ManagementEffectScopeV1 {
                project: effects.project_scope_hash(),
                session: None,
                generation: 0,
                revision: effects.cursor()?.generation(),
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(request),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) =
            effects.prepare(&self.lease, effects.cursor()?, request)?
        else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        if !accepted {
            effects.deny(
                &self.lease,
                &prepared,
                ManagementDenialReasonV1::UserDeclined,
            )?;
            return Ok(None);
        }
        let started = effects.start(&self.lease, &prepared)?;
        match change() {
            Ok(value) => {
                let receipt = match crate::sync::participation::read_snapshot(&self.state_dir) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        effects.mark_outcome_unknown(
                            &self.lease,
                            &started,
                            ManagementUnknownReasonV1::UnclassifiedEffect,
                        )?;
                        return Err(error.into());
                    }
                };
                let proof =
                    serde_json::to_vec(&receipt.exact_digest).map_err(std::io::Error::other)?;
                effects.complete_started(
                    &self.lease,
                    &started,
                    ManagementEffectHashV1::digest(&proof),
                )?;
                Ok(Some(value))
            }
            Err(error) => {
                effects.mark_outcome_unknown(
                    &self.lease,
                    &started,
                    ManagementUnknownReasonV1::UnclassifiedEffect,
                )?;
                Err(error)
            }
        }
    }

    pub fn join_confirmed(
        &self,
        _share: crate::sync::share::ShareLink,
        _manifest: crate::sync::share::ShareManifest,
        _now: u64,
        _accepted: bool,
    ) -> Result<Option<crate::sync::participation::Participation>, ApplyOwnerError> {
        // Do not create a terminal C08 effect without the C08 intent owner.
        Err(ApplyOwnerError::EvidenceMismatch)
    }

    pub fn leave_confirmed(&self, accepted: bool) -> Result<Option<bool>, ApplyOwnerError> {
        use crate::sync::participation;
        let held = participation::read_snapshot(&self.state_dir)?;
        if held.classification.participation().is_none() {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let bytes = serde_json::to_vec(&held.exact_digest).map_err(std::io::Error::other)?;
        self.sync_change("C13", &bytes, accepted, || {
            let inventory =
                participation::C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
            Ok(participation::leave_under_owner(&inventory)?)
        })
    }

    pub fn pull_confirmed(
        &self,
        accepted: bool,
    ) -> Result<Option<crate::sync::exchange::Exchange>, ApplyOwnerError> {
        use crate::sync::{participation, repository::SyncRepository, share::SyncPaths};
        let snapshot = participation::read_snapshot(&self.state_dir)?;
        let mut held = snapshot
            .classification
            .participation()
            .cloned()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let bytes = serde_json::to_vec(&snapshot.exact_digest).map_err(std::io::Error::other)?;
        self.sync_change("C11", &bytes, accepted, || {
            let repository = SyncRepository::open(SyncPaths::under(&self.state_dir))?;
            repository.stage_from(&self.project_root, &held.manifest)?;
            repository.commit("polaris sync")?;
            repository.receive(&held.share.remote)?;
            let exchange = repository.merge_received()?;
            held.unresolved = exchange
                .needs_attention()
                .iter()
                .map(|c| c.path.clone())
                .collect();
            held.agreed_commit = exchange.merged_commit.clone().or(held.agreed_commit);
            if exchange.changes.is_empty() && !exchange.unresolved {
                held.applied_commit = held.agreed_commit.clone();
            }
            let inventory =
                participation::C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
            participation::write_under_owner(&inventory, &held)?;
            Ok(exchange)
        })
    }

    pub fn apply_c12(
        &self,
        admission: C12ApplyAdmissionV1,
        snapshot: &crate::isolated_workspace::Snapshot,
        changes: &crate::isolated_workspace::ChangeSet,
        limits: crate::isolated_workspace::Limits,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<crate::workspace_apply::ApplyReport, ApplyOwnerError> {
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.apply_c12_protected(admission, snapshot, changes, limits, parent, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
    }

    fn apply_c12_protected(
        &self,
        admission: C12ApplyAdmissionV1,
        snapshot: &crate::isolated_workspace::Snapshot,
        changes: &crate::isolated_workspace::ChangeSet,
        limits: crate::isolated_workspace::Limits,
        parent: &crate::workspace_apply::RecoveryParent,
        secrets: &crate::isolated_workspace::RegisteredSecrets,
    ) -> Result<crate::workspace_apply::ApplyReport, ApplyOwnerError> {
        use crate::apply_intent::*;
        use crate::project_mutation::open_directory_nofollow;
        use crate::sync::participation::*;
        use crate::workspace_apply as workspace;
        use std::os::unix::fs::MetadataExt;
        self.ensure_sync_idle()?;
        if self.active()?.is_some()
            || snapshot.source_path != self.project_root
            || !changes.is_applicable()
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        parent
            .validate_for_snapshot(snapshot)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let request = request_from_changes(snapshot, changes)?;
        let before = self.observe_source(&request, secrets)?;
        before.verify_before(&request)?;
        let inventory = C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
        let (base, initializing) = match inventory.inspect()? {
            C12InspectionV1::Legacy { participation_hash } => (
                ParticipationGenerationV1 {
                    schema_version: 1,
                    generation: 0,
                    mutation_id: None,
                    participation_hash,
                    previous_marker_hash: None,
                    publication: ParticipationPublicationV1::Baseline,
                },
                true,
            ),
            C12InspectionV1::Committed {
                marker,
                pending: None,
            } => (marker, false),
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        };
        let current_bytes = inventory
            .current_body()?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let current_snapshot = decode_participation_v2(&current_bytes)?;
        let current = current_snapshot
            .participation()
            .cloned()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let target = decode_participation_v2(&admission.target_participation)?;
        let mut expected = current.clone();
        expected.applied_commit = current.agreed_commit.clone();
        let expected_snapshot =
            decode_participation_v2(&updated_participation_bytes_v2(&current_bytes, &expected)?)?;
        if !current.unresolved.is_empty()
            || target != expected_snapshot
            || current.agreed_commit.as_deref() != Some(admission.agreed_revision.value.as_str())
            || current.applied_commit.as_deref()
                != admission
                    .previous_applied_revision
                    .as_ref()
                    .map(|v| v.value.as_str())
            || admission.agreed_revision != admission.staged_revision
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if request
            .entries
            .iter()
            .any(|entry| !current.manifest.shares(Path::new(&entry.path)))
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let project_binding =
            ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes());
        let operation_key = c12_operation_key(project_binding, admission.management_operation_id)?;
        let target_marker = ParticipationGenerationV1 {
            schema_version: 1,
            generation: base
                .generation
                .checked_add(1)
                .ok_or(ApplyOwnerError::EvidenceMismatch)?,
            mutation_id: Some(operation_key),
            participation_hash: Some(participation_bytes_hash(&admission.target_participation)),
            previous_marker_hash: Some(base.marker_hash()?),
            publication: ParticipationPublicationV1::Apply,
        };
        let state_directory = open_directory_nofollow(&self.state_dir)?;
        let recovery_directory = open_directory_nofollow(parent.provenance())?;
        let recovery_metadata = recovery_directory.metadata()?;
        if (recovery_metadata.dev(), recovery_metadata.ino()) != parent.identity() {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let roots =
            CapacityRootsV1::from_pinned_files(&state_directory, &recovery_directory, None)?;
        let workspace_capacity = workspace::capacity_upper_bound(
            &request,
            &self.project_root,
            &directory_chain(&self.project_root)?,
            parent.provenance(),
            &directory_chain(parent.provenance())?,
            operation_key,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let inventory_capacity = c12_mutation_capacity_upper_bound(
            &base,
            ParticipationPublicationV1::Apply,
            Some(&admission.target_participation),
            roots.state.allocation_unit,
            initializing,
        )?;
        let inspected = self.inspect_management()?;
        if let ManagementEffectInspectOpenV1::Present(value) = &inspected
            && value
                .effects()
                .any(|effect| effect.request.operation_id == admission.management_operation_id)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        require_absent_child(&recovery_directory, &recovery_child(operation_key)?)?;
        let (record_limit, records_remaining, bytes_remaining) = match &inspected {
            ManagementEffectInspectOpenV1::Present(value) => value.fresh_admission_limits()?,
            ManagementEffectInspectOpenV1::Absent => {
                let value = ManagementEffectLimitsV1::default();
                (
                    value.max_record_bytes as u64,
                    value.max_records as u64,
                    value.max_journal_bytes,
                )
            }
        };
        let (archive_count, archive_file_bytes) = self.intents()?.archive_inventory()?;
        // Descriptor hashes are fixed width in the management encoder. Build
        // the plan with that width, then verify the real request has the same
        // production lengths after descriptor hashing (no circular hashing).
        let prepare = PrepareManagementEffectV1 {
            operation_id: admission.management_operation_id,
            inventory: ManagementInventoryIdV1::parse("C12").expect("fixed inventory"),
            scope: ManagementEffectScopeV1 {
                project: project_binding,
                session: None,
                generation: base.generation,
                revision: target_marker.generation,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(b"fixed-width admission hash"),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let build_capacity = |prepare: &PrepareManagementEffectV1| {
            CapacityPlanV1::build(CapacityAdmissionV1 {
                roots: roots.clone(),
                request: &request,
                source_before: &before,
                d27_source_binding: None,
                prepare,
                management_record_limit: record_limit,
                management_records_remaining: records_remaining,
                management_bytes_remaining: bytes_remaining,
                inventory_commit_bytes: inventory_capacity.required_bytes,
                journal_bytes: workspace_capacity.journal_bytes,
                report_bytes: workspace_capacity.report_bytes,
                archive_seal_bytes: MAX_INTENT_GENERATION_BYTES,
                desktop_result_required_bytes: None,
                desktop_records_remaining: None,
                desktop_bytes_remaining: None,
                archive_count,
                archive_file_bytes,
            })
        };
        let plan = build_capacity(&prepare)?;
        let descriptor = ApplyIntentDescriptorV1 {
            schema_version: 1,
            operation_key,
            management_operation_id: admission.management_operation_id,
            inventory_id: ApplyInventoryV1::C12,
            project_binding,
            source_identity: request.source_identity,
            agreed_revision: admission.agreed_revision,
            staged_revision: admission.staged_revision,
            previous_applied_revision: admission.previous_applied_revision,
            context: ApplyContextV1::Participation {
                base_generation: base.generation,
                target_generation: target_marker.generation,
                base_marker_hash: base.marker_hash()?,
                mutation_id: operation_key,
                target_participation_hash: target_marker.participation_hash,
                target_marker_hash: target_marker.marker_hash()?,
            },
            share_source_manifest_digest: ManagementEffectHashV1::from_hex(
                &target
                    .participation()
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?
                    .manifest
                    .digest(),
            )
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            changed_path_count: request.entries.len() as u64,
            path_set_digest: request.path_set_digest()?,
            workspace_apply_request_digest: request.request_hash()?,
            source_before_observation_digest: before.observation_hash()?,
            d27_source_binding_bytes: None,
            d27_source_binding_hash: None,
            recovery_parent_identity: roots.recovery.identity,
            recovery_provenance_digest: recovery_digest(operation_key)?,
            recovery_child: recovery_child(operation_key)?,
            capacity_plan_bytes: plan.canonical_bytes()?.len() as u64,
            capacity_plan_hash: plan.plan_hash()?,
            capacity_plan: plan.clone(),
            phase: ApplyPhaseV1::Prepared,
        };
        let prepare = Self::management_request(&descriptor)?;
        if build_capacity(&prepare)? != plan || self.observe_source(&request, secrets)? != before {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        self.require_live_capacity(
            &plan,
            &recovery_directory,
            None,
            plan.state_required_bytes,
            plan.recovery_required_bytes,
            None,
        )?;
        let descriptor_hash = descriptor.descriptor_hash()?;
        let zero = ApplyGenerationV1 {
            schema_version: 1,
            sequence: 0,
            previous_record_hash: ManagementEffectHashV1::from_hex(&"0".repeat(64))
                .expect("fixed zero"),
            descriptor_hash,
            phase: ApplyPhaseV1::Prepared,
            evidence: ApplyEvidenceV1::Prepared {
                capacity_plan_hash: descriptor.capacity_plan_hash,
                request_hash: request.request_hash()?,
                request_bytes: request.canonical_bytes()?.len() as u64,
                source_before_hash: before.observation_hash()?,
                source_before_bytes: before.canonical_bytes()?.len() as u64,
            },
        };
        self.intents()?.publish_generation_zero(
            ApplyIntentEnvelopeV1 {
                schema_version: 1,
                descriptor: Some(descriptor),
                descriptor_hash,
                record_hash: zero.record_hash()?,
                generation: zero,
            },
            &request,
            &before,
            None,
        )?;
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        #[cfg(test)]
        owner_stop_after(ApplyPhaseV1::Prepared)?;
        let store = match inspected {
            ManagementEffectInspectOpenV1::Present(value) => {
                value.admit_fresh(&self.lease, &intent)?
            }
            ManagementEffectInspectOpenV1::Absent => {
                ManagementEffectStoreV1::initialize_fresh_joined(
                    &self.project_root,
                    &self.state_dir,
                    &self.lease,
                    &intent,
                )?
            }
        };
        if initializing
            && inventory.initialize_for_mutation(
                ParticipationPublicationV1::Apply,
                Some(&admission.target_participation),
            )? != base
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let prepared = match store.prepare(&self.lease, store.cursor()?, prepare)? {
            PrepareManagementEffectOutcomeV1::Prepared(prepared) => prepared,
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        };
        let inventory_remaining = c12_mutation_capacity_upper_bound(
            &base,
            ParticipationPublicationV1::Apply,
            Some(&admission.target_participation),
            plan.state_allocation_unit,
            false,
        )?
        .required_bytes;
        self.require_live_capacity(
            &plan,
            &recovery_directory,
            None,
            Self::remaining_state_capacity(&intent, &ManagementEffectStateV1::Prepared, false)?
                .checked_add(inventory_remaining)
                .ok_or(ApplyIntentError::Overflow)?,
            plan.recovery_required_bytes,
            None,
        )?;
        let reserved = workspace::prepare_reserved_recovery_v2(
            snapshot,
            changes,
            secrets,
            limits,
            parent,
            operation_key,
            descriptor_hash,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let child = reserved.directory().metadata()?;
        let proof = prepared.verified_event().evidence;
        self.append_active(ApplyEvidenceV1::RecoveryPrepared {
            child_identity: IdentityV1 {
                device: child.dev(),
                inode: child.ino(),
            },
            journal_header_hash: reserved
                .header()
                .header_hash()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            journal_header_bytes: reserved
                .header()
                .canonical_bytes()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
                .len() as u64,
            directory_synced: true,
            generic_prepared_generation: proof.generation,
            generic_prepared_hash: proof.event_hash,
            generic_prepared_sequence: proof.sequence,
        })?;
        let started = store.start(&self.lease, &prepared)?;
        let proof = started.verified_event().evidence;
        self.append_active(ApplyEvidenceV1::EffectStarted {
            generic_generation: proof.generation,
            generic_event_hash: proof.event_hash,
            generic_sequence: proof.sequence,
        })?;
        let report = workspace::apply_reserved_v2(snapshot, changes, secrets, limits, &reserved);
        let report_hash = report
            .publish_exact(reserved.directory())
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let report_bytes = report
            .canonical_bytes()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
            .len() as u64;
        let journal = reserved
            .inspect_journal(plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let tip = workspace::journal_tip_hash(&journal, plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let success = report.failure.is_none()
            && report.entries.len() == request.entries.len()
            && report
                .entries
                .iter()
                .zip(&request.entries)
                .all(|(reported, requested)| {
                    reported.relative_path == Path::new(&requested.path)
                        && if requested.after.is_some() {
                            reported.installed
                        } else {
                            reported.deleted
                        }
                });
        let result = if success {
            ApplyResultV1::Success
        } else if report
            .entries
            .iter()
            .any(|e| e.installed || e.deleted || e.old_retained)
        {
            ApplyResultV1::Partial
        } else {
            ApplyResultV1::Failed
        };
        self.append_active(ApplyEvidenceV1::ApplyReported {
            report_hash,
            report_bytes,
            journal_tip_hash: tip,
            result,
        })?;
        if !success {
            let unknown = store.mark_outcome_unknown(
                &self.lease,
                &started,
                ManagementUnknownReasonV1::UnclassifiedEffect,
            )?;
            let proof = unknown.verified_event().evidence;
            self.append_active(ApplyEvidenceV1::OutcomeUnknown {
                reason: ManagementUnknownReasonV1::UnclassifiedEffect,
                journal_tip_hash: Some(tip),
                report_hash: Some(report_hash),
                marker_hash: None,
                generic_generation: proof.generation,
                generic_event_hash: proof.event_hash,
                generic_sequence: proof.sequence,
            })?;
            return Ok(report);
        }
        self.observe_source(&request, secrets)?
            .verify_after(&request)?;
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        self.require_live_capacity(
            &plan,
            &recovery_directory,
            None,
            Self::remaining_state_capacity(
                &intent,
                &ManagementEffectStateV1::EffectStarted,
                false,
            )?
            .checked_add(inventory_remaining)
            .ok_or(ApplyIntentError::Overflow)?,
            0,
            None,
        )?;
        let pending = inventory.publish_apply_pending(
            &base,
            &admission.target_participation,
            operation_key,
        )?;
        if pending.target_marker != target_marker
            || inventory.resume_pending()? != target_marker
            || inventory.current_body()?.as_deref()
                != Some(admission.target_participation.as_slice())
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let marker_hash = target_marker.marker_hash()?;
        self.append_active(ApplyEvidenceV1::ParticipationCommitted {
            marker_kind: MarkerKindV1::C12Generation,
            marker_hash,
            committed_revision: target_marker.generation,
            sidecar_hash: target_marker.participation_hash,
            sidecar_bytes: Some(admission.target_participation.len() as u64),
            result_id_digest: None,
        })?;
        let source_after = self.observe_source(&request, secrets)?;
        source_after.verify_after(&request)?;
        // Final evidence is freshly reopened; the in-memory report is not a
        // substitute for the published recovery artifacts.
        let journal_after = reserved
            .inspect_journal(plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if workspace::journal_tip_hash(&journal_after, plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
            != tip
            || read_report_hash(reserved.directory(), plan.report_bytes)? != report_hash
            || inventory.inspect()?
                != (C12InspectionV1::Committed {
                    marker: target_marker.clone(),
                    pending: None,
                })
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let readback = FinalReadbackV1 {
            schema_version: 1,
            descriptor_hash,
            request_hash: request.request_hash()?,
            source_observation_hash: source_after.observation_hash()?,
            journal_tip_hash: Some(tip),
            report_hash: Some(report_hash),
            marker_kind: Some(MarkerKindV1::C12Generation),
            marker_hash: Some(marker_hash),
            agreed_revision: intent.descriptor().agreed_revision.clone(),
        };
        let completed = store.complete_started(&self.lease, &started, readback.readback_hash()?)?;
        let proof = completed.verified_event().evidence;
        self.append_active(ApplyEvidenceV1::Completed {
            readback_hash: readback.readback_hash()?,
            generic_generation: proof.generation,
            generic_event_hash: proof.event_hash,
            generic_sequence: proof.sequence,
        })?;
        self.archive_terminal_joined(Some(&store))?;
        Ok(report)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn begin_d27(
        self,
        writer: &mut crate::desktop_store::Writer,
        admission: D27ApplyAdmissionV1,
        snapshot: &crate::isolated_workspace::Snapshot,
        changes: &crate::isolated_workspace::ChangeSet,
        limits: crate::isolated_workspace::Limits,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<D27ApplyOperationV1, ApplyOwnerError> {
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.begin_d27_protected(
                writer,
                admission,
                (snapshot, changes),
                limits,
                parent,
                &secrets,
            )
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn begin_d27_protected(
        self,
        writer: &mut crate::desktop_store::Writer,
        admission: D27ApplyAdmissionV1,
        (snapshot, changes): (
            &crate::isolated_workspace::Snapshot,
            &crate::isolated_workspace::ChangeSet,
        ),
        limits: crate::isolated_workspace::Limits,
        parent: &crate::workspace_apply::RecoveryParent,
        secrets: &crate::isolated_workspace::RegisteredSecrets,
    ) -> Result<D27ApplyOperationV1, ApplyOwnerError> {
        use crate::apply_intent::*;
        use crate::project_mutation::open_directory_nofollow;
        use crate::workspace_apply as workspace;
        use std::os::unix::fs::MetadataExt;
        self.ensure_sync_idle()?;
        if snapshot.source_path != self.project_root {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        parent
            .validate_for_snapshot(snapshot)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        require_root_encoding(parent.provenance(), RootKindV1::RecoveryParent)?;
        let (desktop_path, desktop_file) = writer.retained_root_m2a()?;
        require_root_encoding(&desktop_path, RootKindV1::DesktopRoot)?;
        let saved = writer.snapshot()?;
        let record = saved
            .state
            .source_applies
            .iter()
            .find(|record| {
                record.candidate.run_id == admission.target.run_id
                    && record.candidate.attempt_id == admission.target.attempt_id
                    && record.candidate.approval_id == admission.approval_id
            })
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let artifacts = crate::isolated_workspace::project_d27_source_m2a(
            snapshot, changes, record, parent, secrets, limits,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let request = artifacts.request;
        let before = artifacts.source_before;
        let binding = artifacts.binding;
        if self.observe_source(&request, secrets)? != before {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let project_binding =
            ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes());
        let operation = d27_operation_binding(record.candidate.operation_id.as_str())?;
        let operation_key = d27_operation_key(project_binding, operation)?;
        let management_id = d27_management_operation_id(operation)?;
        let workspace_capacity = workspace::capacity_upper_bound(
            &request,
            &self.project_root,
            &directory_chain(&self.project_root)?,
            parent.provenance(),
            &directory_chain(parent.provenance())?,
            operation_key,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let result_capacity = crate::desktop_store::source_apply_result_capacity_m2a(
            &admission.result_id,
            workspace_capacity.report_bytes,
        )?;
        let token = writer.precommit_source_apply_m2a(
            &admission.target,
            &admission.approval_id,
            &admission.payload_hash,
            (admission.guard, admission.proof),
            project_binding,
            (admission.result_id, result_capacity),
        )?;
        let precommit = token.precommit().clone();
        let state_file = open_directory_nofollow(&self.state_dir)?;
        let recovery_file = open_directory_nofollow(parent.provenance())?;
        let recovery_metadata = recovery_file.metadata()?;
        if (recovery_metadata.dev(), recovery_metadata.ino()) != parent.identity() {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let roots =
            CapacityRootsV1::from_pinned_files(&state_file, &recovery_file, Some(&desktop_file))?;
        let inspected = self.inspect_management()?;
        if let ManagementEffectInspectOpenV1::Present(value) = &inspected
            && value
                .effects()
                .any(|effect| effect.request.operation_id == management_id)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        require_absent_child(&recovery_file, &recovery_child(operation_key)?)?;
        let (record_limit, records_remaining, bytes_remaining) = match &inspected {
            ManagementEffectInspectOpenV1::Present(value) => value.fresh_admission_limits()?,
            ManagementEffectInspectOpenV1::Absent => {
                let limits = ManagementEffectLimitsV1::default();
                (
                    limits.max_record_bytes as u64,
                    limits.max_records as u64,
                    limits.max_journal_bytes,
                )
            }
        };
        let logical = token.logical_capacity();
        let inventory_bytes = token.inventory_commit_bytes()?;
        let (archive_count, archive_file_bytes) = self.intents()?.archive_inventory()?;
        let prepare = PrepareManagementEffectV1 {
            operation_id: management_id,
            inventory: ManagementInventoryIdV1::parse("D27").expect("fixed inventory"),
            scope: ManagementEffectScopeV1 {
                project: project_binding,
                session: Some(d27_session_binding(saved.marker.session_id.as_str())?),
                generation: saved.marker.epoch.get(),
                revision: precommit.consume_target_revision,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(b"fixed-width admission hash"),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let build_capacity = |prepare: &PrepareManagementEffectV1| {
            CapacityPlanV1::build(CapacityAdmissionV1 {
                roots: roots.clone(),
                request: &request,
                source_before: &before,
                d27_source_binding: Some(&binding),
                prepare,
                management_record_limit: record_limit,
                management_records_remaining: records_remaining,
                management_bytes_remaining: bytes_remaining,
                inventory_commit_bytes: inventory_bytes,
                journal_bytes: workspace_capacity.journal_bytes,
                report_bytes: workspace_capacity.report_bytes,
                archive_seal_bytes: MAX_INTENT_GENERATION_BYTES,
                desktop_result_required_bytes: Some(result_capacity),
                desktop_records_remaining: Some(logical.records_remaining),
                desktop_bytes_remaining: Some(logical.bytes_remaining),
                archive_count,
                archive_file_bytes,
            })
        };
        let plan = build_capacity(&prepare)?;
        let descriptor = ApplyIntentDescriptorV1 {
            schema_version: 1,
            operation_key,
            management_operation_id: management_id,
            inventory_id: ApplyInventoryV1::D27,
            project_binding,
            source_identity: request.source_identity,
            agreed_revision: RevisionV1 {
                kind: RevisionKindV1::Sha256,
                value: binding.payload_hash.as_hex(),
            },
            staged_revision: RevisionV1 {
                kind: RevisionKindV1::Sha256,
                value: request.request_hash()?.as_hex(),
            },
            previous_applied_revision: None,
            context: ApplyContextV1::Session {
                base_epoch: saved.marker.epoch.get(),
                session_binding: d27_session_binding(saved.marker.session_id.as_str())?,
                base_session_revision: precommit.base_session_revision,
                consume_target_revision: precommit.consume_target_revision,
                policy_revision: precommit.policy_revision,
                source_operation_digest: operation,
                reserved_result_id: precommit.reserved_result_id.as_str().into(),
                reserved_result_id_digest: precommit.reserved_result_id_digest,
                base_marker_evidence_hash: precommit.base_marker_evidence_hash,
                consume_marker_evidence_hash: precommit.consume_marker_evidence_hash,
                consume_sidecar_offset: precommit.consume_sidecar_offset,
                consume_sidecar_hash: precommit.consume_sidecar_hash,
                precommit_hash: precommit.precommit_hash()?,
            },
            share_source_manifest_digest: binding.manifest_hash,
            changed_path_count: request.entries.len() as u64,
            path_set_digest: request.path_set_digest()?,
            workspace_apply_request_digest: request.request_hash()?,
            source_before_observation_digest: before.observation_hash()?,
            d27_source_binding_bytes: Some(binding.canonical_bytes()?.len() as u64),
            d27_source_binding_hash: Some(binding.binding_hash()?),
            recovery_parent_identity: roots.recovery.identity,
            recovery_provenance_digest: binding.manifest.recovery_provenance_digest()?,
            recovery_child: recovery_child(operation_key)?,
            capacity_plan_bytes: plan.canonical_bytes()?.len() as u64,
            capacity_plan_hash: plan.plan_hash()?,
            capacity_plan: plan.clone(),
            phase: ApplyPhaseV1::Prepared,
        };
        binding.verify_artifacts(&descriptor, &request, &before)?;
        let prepare = Self::management_request(&descriptor)?;
        if build_capacity(&prepare)? != plan
            || self.observe_source(&request, secrets)? != before
            || writer.snapshot()?.marker.session_revision != saved.marker.session_revision
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        self.require_live_capacity(
            &plan,
            &recovery_file,
            Some(&desktop_file),
            plan.state_required_bytes,
            plan.recovery_required_bytes,
            plan.desktop_required_bytes,
        )?;
        let descriptor_hash = descriptor.descriptor_hash()?;
        let zero = ApplyGenerationV1 {
            schema_version: 1,
            sequence: 0,
            previous_record_hash: ManagementEffectHashV1::from_hex(&"0".repeat(64))
                .expect("fixed zero"),
            descriptor_hash,
            phase: ApplyPhaseV1::Prepared,
            evidence: ApplyEvidenceV1::Prepared {
                capacity_plan_hash: descriptor.capacity_plan_hash,
                request_hash: request.request_hash()?,
                request_bytes: request.canonical_bytes()?.len() as u64,
                source_before_hash: before.observation_hash()?,
                source_before_bytes: before.canonical_bytes()?.len() as u64,
            },
        };
        self.intents()?.publish_generation_zero(
            ApplyIntentEnvelopeV1 {
                schema_version: 1,
                descriptor: Some(descriptor),
                descriptor_hash,
                record_hash: zero.record_hash()?,
                generation: zero,
            },
            &request,
            &before,
            Some(&binding),
        )?;
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let store = match inspected {
            ManagementEffectInspectOpenV1::Present(value) => {
                value.admit_fresh(&self.lease, &intent)?
            }
            ManagementEffectInspectOpenV1::Absent => {
                ManagementEffectStoreV1::initialize_fresh_joined(
                    &self.project_root,
                    &self.state_dir,
                    &self.lease,
                    &intent,
                )?
            }
        };
        let prepared = match store.prepare(&self.lease, store.cursor()?, prepare)? {
            PrepareManagementEffectOutcomeV1::Prepared(prepared) => prepared,
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        };
        let desktop_result_remaining_bytes = token.result_remaining_bytes()?;
        self.require_live_capacity(
            &plan,
            &recovery_file,
            Some(&desktop_file),
            Self::remaining_state_capacity(&intent, &ManagementEffectStateV1::Prepared, false)?,
            plan.recovery_required_bytes,
            Some(token.inventory_commit_bytes()?),
        )?;
        let consumed = writer.consume_source_apply_precommitted_m2a(
            token,
            admission.guard,
            admission.proof,
        )?;
        if consumed.precommit != precommit {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        Ok(D27ApplyOperationV1 {
            owner: Some(self),
            desktop_root_directory: desktop_file,
            desktop_result_remaining_bytes,
            coordinates: (saved.marker.project_id, saved.marker.session_id),
            precommit,
            descriptor: intent.descriptor().clone(),
            store,
            prepared: Some(prepared),
            started: None,
            reserved: None,
            reported: false,
            completed: false,
        })
    }

    fn append_active(&self, evidence: ApplyEvidenceV1) -> Result<(), ApplyOwnerError> {
        #[cfg(any(test, feature = "execution-test-support"))]
        let phase = evidence.phase();
        self.append_evidence(
            &self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?,
            evidence,
        )?;
        #[cfg(any(test, feature = "execution-test-support"))]
        owner_stop_after(phase)?;
        Ok(())
    }

    fn archive_terminal_joined(
        &self,
        store: Option<&ManagementEffectStoreV1>,
    ) -> Result<(), ApplyOwnerError> {
        use crate::apply_intent::*;
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let terminal = intent.latest();
        if !terminal.phase.is_terminal() {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let readback_hash = match terminal.evidence {
            ApplyEvidenceV1::Completed { readback_hash, .. }
            | ApplyEvidenceV1::ReconciledAbandon { readback_hash, .. } => Some(readback_hash),
            _ => None,
        };
        let generic = terminal.evidence.generic_evidence();
        match (store, generic) {
            (Some(store), Some(_)) => {
                let snapshot = store
                    .effect(intent.descriptor().management_operation_id)?
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?;
                let verified = store
                    .latest_event_evidence(snapshot.request.operation_id, &snapshot)?
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?;
                Self::verify_generic_join(&intent, &snapshot, &verified)?;
            }
            (Some(store), None)
                if store
                    .effect(intent.descriptor().management_operation_id)?
                    .is_none() => {}
            (None, None) => {}
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        }
        let mut seal = ArchiveSealV1 {
            schema_version: 1,
            operation_key: intent.descriptor().operation_key,
            descriptor_hash: intent.descriptor().descriptor_hash()?,
            terminal_sequence: terminal.sequence,
            terminal_record_hash: terminal.record_hash()?,
            terminal_phase: terminal.phase,
            request_hash: intent.request.request_hash()?,
            source_before_hash: intent.source_before.observation_hash()?,
            journal_header_hash: None,
            journal_tip_hash: None,
            report_hash: None,
            marker_kind: None,
            marker_hash: None,
            generic_terminal_generation: generic.map(|v| v.0),
            generic_terminal_hash: generic.map(|v| v.1),
            generic_terminal_sequence: generic.map(|v| v.2),
            final_readback_hash: readback_hash,
        };
        for envelope in &intent.generations {
            match envelope.generation.evidence {
                ApplyEvidenceV1::RecoveryPrepared {
                    journal_header_hash,
                    ..
                } => seal.journal_header_hash = Some(journal_header_hash),
                ApplyEvidenceV1::ApplyReported {
                    journal_tip_hash,
                    report_hash,
                    ..
                } => {
                    seal.journal_tip_hash = Some(journal_tip_hash);
                    seal.report_hash = Some(report_hash);
                }
                ApplyEvidenceV1::ParticipationCommitted {
                    marker_kind,
                    marker_hash,
                    ..
                } => {
                    seal.marker_kind = Some(marker_kind);
                    seal.marker_hash = Some(marker_hash);
                }
                _ => {}
            }
        }
        let plan = &intent.descriptor().capacity_plan;
        let state = crate::project_mutation::open_directory_nofollow(&self.state_dir)?;
        let sample = CapacityRootsV1::from_pinned_files(&state, &state, None)?.state;
        if sample.identity != plan.state_identity
            || sample.allocation_unit != plan.state_allocation_unit
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let remaining = Self::remaining_state_capacity(
            &intent,
            &ManagementEffectStateV1::Completed {
                evidence_hash: terminal.record_hash()?,
            },
            false,
        )?;
        #[cfg(any(test, feature = "execution-test-support"))]
        let available = OWNER_AVAILABLE_LIMIT
            .with(|slot| slot.get())
            .map_or(sample.available_bytes, |limit| {
                sample.available_bytes.min(limit)
            });
        #[cfg(not(any(test, feature = "execution-test-support")))]
        let available = sample.available_bytes;
        if remaining
            .checked_add(FREE_SPACE_FLOOR_BYTES)
            .ok_or(ApplyIntentError::Overflow)?
            > available
        {
            return Err(ApplyIntentError::Capacity("terminal archive").into());
        }
        self.intents()?
            .archive_terminal(seal.operation_key, &seal)?;
        Ok(())
    }

    pub fn acquire(project_root: &Path) -> Result<Self, ApplyOwnerError> {
        let project_root = std::fs::canonicalize(project_root)?;
        crate::project_mutation::open_directory_nofollow(&project_root)?;
        crate::apply_intent::require_root_encoding(
            &project_root,
            crate::apply_intent::RootKindV1::Project,
        )?;
        let state_dir = crate::project::state_dir(&project_root)?;
        let state_dir = std::fs::canonicalize(state_dir)?;
        // The project state is created by the host. Tighten this verified
        // owner-owned directory through its descriptor before opening owners.
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let directory = crate::project_mutation::open_directory_nofollow(&state_dir)?;
            let metadata = directory.metadata()?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7000 != 0 {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            directory.sync_all()?;
            let fresh = crate::project_mutation::open_directory_nofollow(&state_dir)?.metadata()?;
            if (metadata.dev(), metadata.ino()) != (fresh.dev(), fresh.ino())
                || fresh.mode() & 0o7777 != 0o700
            {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
        }
        crate::apply_intent::require_root_encoding(
            &state_dir,
            crate::apply_intent::RootKindV1::State,
        )?;
        let owner = ProjectMutationOwnerV1::at_store_root(&state_dir.join("rewind-workspace"))?;
        Self::with_lease(&project_root, &state_dir, owner.try_mutation()?)
    }

    pub(crate) fn with_lease(
        project_root: &Path,
        state_dir: &Path,
        lease: ProjectMutationLeaseV1,
    ) -> Result<Self, ApplyOwnerError> {
        let state_dir = std::fs::canonicalize(state_dir)?;
        // This inspection neither creates an intent nor opens the mutating
        // management store. Each writer borrows the retained lease briefly.
        ApplyIntentStoreV1::inspect(&state_dir, &lease)?;
        Ok(Self {
            project_root: std::fs::canonicalize(project_root)?,
            state_dir,
            lease,
        })
    }

    fn intents(&self) -> Result<ApplyIntentStoreV1<'_>, ApplyIntentError> {
        ApplyIntentStoreV1::inspect(&self.state_dir, &self.lease)
    }

    pub fn active(&self) -> Result<Option<InspectedApplyIntentV1>, ApplyOwnerError> {
        Ok(self.intents()?.active()?)
    }

    pub fn inspect_management(&self) -> Result<ManagementEffectInspectOpenV1, ApplyOwnerError> {
        Ok(ManagementEffectStoreV1::inspect_open_at_state_dir(
            &self.project_root,
            &self.state_dir,
            &self.lease,
        )?)
    }

    /// Startup discovers durable evidence under the retained lease. It never
    /// dispatches the workspace effect again. Completion requires a fresh
    /// readback of an already committed C12 generation and all recovery bytes.
    pub fn recover_current(
        &self,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<ApplyOwnerRecoveryV1, ApplyOwnerError> {
        use crate::apply_intent::{ApplyPhaseV1, ChildDispositionV1};
        let Some(intent) = self.active()? else {
            return Ok(ApplyOwnerRecoveryV1::Idle);
        };
        if intent.descriptor().project_binding
            != ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes())
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        parent
            .validate_identity()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if parent.identity()
            != (
                intent.descriptor().recovery_parent_identity.device,
                intent.descriptor().recovery_parent_identity.inode,
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let key = intent.descriptor().operation_key;
        let inspected = self.inspect_management()?;
        if let ManagementEffectInspectOpenV1::Present(value) = &inspected
            && value.effects().any(|e| {
                e.request.operation_id != intent.descriptor().management_operation_id
                    && e.state.blocks_new_work()
            })
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if intent.latest().phase.is_terminal() {
            if let ApplyEvidenceV1::Completed { readback_hash, .. } = intent.latest().evidence {
                if self
                    .c12_committed_readback(&intent, parent)?
                    .readback_hash()?
                    != readback_hash
                {
                    return Err(ApplyOwnerError::EvidenceMismatch);
                }
            } else {
                self.verify_source_before(&intent)?;
                self.inspect_header_child(&intent, parent)?;
            }
            match inspected {
                ManagementEffectInspectOpenV1::Present(value) => {
                    let store = value.joined_terminal_store(&intent)?;
                    self.archive_terminal_joined(Some(&store))?;
                }
                ManagementEffectInspectOpenV1::Absent
                    if intent.latest().evidence.generic_evidence().is_none() =>
                {
                    self.archive_terminal_joined(None)?
                }
                _ => return Err(ApplyOwnerError::EvidenceMismatch),
            }
            return Ok(ApplyOwnerRecoveryV1::Archived(key));
        }
        let has_operation = match &inspected {
            ManagementEffectInspectOpenV1::Absent => false,
            ManagementEffectInspectOpenV1::Present(value) => value
                .effects()
                .any(|e| e.request.operation_id == intent.descriptor().management_operation_id),
        };
        if !has_operation {
            if intent.latest().phase != ApplyPhaseV1::Prepared {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            self.verify_source_before(&intent)?;
            let disposition = self.inspect_header_child(&intent, parent)?;
            if disposition != ChildDispositionV1::Absent {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            self.append_evidence(
                &intent,
                ApplyEvidenceV1::AbandonedBeforeEffect {
                    source_readback_hash: intent.source_before.observation_hash()?,
                    child_disposition: disposition,
                    generic_denial_reason: None,
                    generic_generation: None,
                    generic_event_hash: None,
                    generic_sequence: None,
                },
            )?;
            let terminal = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
            match inspected {
                ManagementEffectInspectOpenV1::Present(value) => {
                    let store = value.joined_terminal_store(&terminal)?;
                    self.archive_terminal_joined(Some(&store))?;
                }
                ManagementEffectInspectOpenV1::Absent => self.archive_terminal_joined(None)?,
            }
            return Ok(ApplyOwnerRecoveryV1::Archived(key));
        }
        let ManagementEffectInspectOpenV1::Present(inspected) = inspected else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        let committed = intent.generations.iter().any(|e| {
            matches!(
                e.generation.evidence,
                ApplyEvidenceV1::ParticipationCommitted { .. }
            )
        });
        let readback = if committed {
            Some(self.c12_committed_readback(&intent, parent)?)
        } else {
            None
        };
        let completed = inspected.effects().any(|e| {
            e.request.operation_id == intent.descriptor().management_operation_id
                && matches!(e.state, ManagementEffectStateV1::Completed { .. })
        });
        if completed {
            let readback = readback
                .ok_or(ApplyOwnerError::EvidenceMismatch)?
                .readback_hash()?;
            let (store, event) = inspected.joined_completed_readback(&intent, readback)?;
            self.append_active(ApplyEvidenceV1::Completed {
                readback_hash: readback,
                generic_generation: event.evidence.generation,
                generic_event_hash: event.evidence.event_hash,
                generic_sequence: event.evidence.sequence,
            })?;
            self.archive_terminal_joined(Some(&store))?;
            return Ok(ApplyOwnerRecoveryV1::Archived(key));
        }
        let token = self.join_management_recovery(*inspected)?;
        let (store, event) = self.recover_joined_store(token, parent)?;
        if self
            .active()?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?
            .latest()
            .phase
            .is_terminal()
        {
            self.archive_terminal_joined(Some(&store))?;
            return Ok(ApplyOwnerRecoveryV1::Archived(key));
        }
        if let Some(readback) = readback {
            // The generic unknown transition is durable before completion,
            // and no inventory or workspace mutation is replayed here.
            let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
            let fresh = self.c12_committed_readback(&intent, parent)?;
            if fresh != readback {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            let unknown = store
                .unknown_effect(intent.descriptor().management_operation_id)?
                .ok_or(ApplyOwnerError::EvidenceMismatch)?;
            let complete = store.complete_unknown(&self.lease, &unknown, fresh.readback_hash()?)?;
            let proof = complete.verified_event().evidence;
            self.append_active(ApplyEvidenceV1::Completed {
                readback_hash: fresh.readback_hash()?,
                generic_generation: proof.generation,
                generic_event_hash: proof.event_hash,
                generic_sequence: proof.sequence,
            })?;
            self.archive_terminal_joined(Some(&store))?;
            return Ok(ApplyOwnerRecoveryV1::Archived(key));
        }
        Ok(ApplyOwnerRecoveryV1::OutcomeUnknown(event))
    }

    /// Open an intent-bound session through joined recovery before normal-open
    /// can reconcile or invalidate its evidence. Unknown remains a blocker.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn recover_d27_session(
        self,
        root: &crate::desktop_store::DesktopRoot,
        project: &polaris_desktop_protocol::ids::ProjectId,
        session: &polaris_desktop_protocol::ids::SessionId,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<(crate::desktop_store::Writer, ApplyOwnerRecoveryV1), ApplyOwnerError> {
        use crate::apply_intent::*;
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let descriptor = intent.descriptor();
        if descriptor.inventory_id != ApplyInventoryV1::D27 {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if intent.generations.iter().any(|e| {
            matches!(
                e.generation.evidence,
                ApplyEvidenceV1::ApplyReported {
                    result: ApplyResultV1::Success,
                    ..
                }
            )
        }) {
            return self.recover_d27_result(root, project, session, parent);
        }
        let binding = intent
            .d27_source_binding
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        parent
            .validate_identity()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if parent.provenance() != Path::new(&binding.manifest.recovery_parent_path)
            || parent.identity()
                != (
                    descriptor.recovery_parent_identity.device,
                    descriptor.recovery_parent_identity.inode,
                )
            || descriptor.project_binding
                != ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes())
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let opened = root.recovery_open_m2a(project, session, descriptor.project_binding)?;
        let precommit = Self::restore_d27_precommit(&intent, &opened.view.snapshot)?;
        let joined = opened.join_marker_recovery_m2a(
            &precommit,
            descriptor,
            binding,
            (&intent.request, &intent.source_before),
            &Self::management_request(descriptor)?,
            None,
        )?;
        let desktop_directory = opened.retained_root_directory_m2a()?;
        let recovery_directory = parent.retained_directory()?;
        let (generic_state, seal_needed) = match self.inspect_management()? {
            ManagementEffectInspectOpenV1::Absent => (ManagementEffectStateV1::Prepared, false),
            ManagementEffectInspectOpenV1::Present(inspected) => {
                let state = inspected
                    .effects()
                    .find(|effect| {
                        effect.request.operation_id == descriptor.management_operation_id
                    })
                    .map(|effect| effect.state.clone());
                if let Some(state) = state {
                    let seal = if state.is_terminal() {
                        false
                    } else {
                        self.join_management_recovery(*inspected)?
                            .requires_generation_seal()?
                    };
                    (state, seal)
                } else {
                    (ManagementEffectStateV1::Prepared, false)
                }
            }
        };
        self.require_live_capacity(
            &descriptor.capacity_plan,
            &recovery_directory,
            Some(&desktop_directory),
            Self::remaining_state_capacity(&intent, &generic_state, seal_needed)?,
            0,
            Some(
                joined.remaining_publication_bytes(
                    descriptor
                        .capacity_plan
                        .desktop_allocation_unit
                        .ok_or(ApplyOwnerError::EvidenceMismatch)?,
                )?,
            ),
        )?;
        let outcome = self.recover_current(parent)?;
        let (writer, _, _) = opened.recover_published_m2a(joined)?;
        Ok((writer, outcome))
    }

    /// Result-only fresh-process recovery. No snapshot or dispatch capability
    /// is reconstructed; only joined report, marker and source readback may
    /// finish an already reported successful D27 operation.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn recover_d27_result(
        self,
        root: &crate::desktop_store::DesktopRoot,
        project: &polaris_desktop_protocol::ids::ProjectId,
        session: &polaris_desktop_protocol::ids::SessionId,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<(crate::desktop_store::Writer, ApplyOwnerRecoveryV1), ApplyOwnerError> {
        use crate::apply_intent::*;
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let descriptor = intent.descriptor();
        if descriptor.inventory_id != ApplyInventoryV1::D27
            || descriptor.project_binding
                != ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes())
            || !matches!(
                intent.latest().phase,
                ApplyPhaseV1::ApplyReported
                    | ApplyPhaseV1::ParticipationCommitted
                    | ApplyPhaseV1::OutcomeUnknown
                    | ApplyPhaseV1::Completed
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        parent
            .validate_identity()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if parent.identity()
            != (
                descriptor.recovery_parent_identity.device,
                descriptor.recovery_parent_identity.inode,
            )
            || intent
                .d27_source_binding
                .as_ref()
                .map(|b| Path::new(&b.manifest.recovery_parent_path))
                != Some(parent.provenance())
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let opened = root.recovery_open_m2a(project, session, descriptor.project_binding)?;
        let precommit = Self::restore_d27_precommit(&intent, &opened.view.snapshot)?;
        let child = crate::workspace_apply::inspect_reserved_recovery_v2(
            parent,
            descriptor.operation_key,
            descriptor.descriptor_hash()?,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
        .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        self.verify_recovery_child(&child, &intent)?;
        let bytes = read_report_bytes(child.directory(), descriptor.capacity_plan.report_bytes)?;
        let hash = read_report_hash(child.directory(), descriptor.capacity_plan.report_bytes)?;
        let journal = child
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let tip = crate::workspace_apply::journal_tip_hash(
            &journal,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if !intent.generations.iter().any(|event| matches!(event.generation.evidence,
            ApplyEvidenceV1::ApplyReported { report_hash, journal_tip_hash, result: ApplyResultV1::Success, .. }
                if report_hash == hash && journal_tip_hash == tip))
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        // A changed source must refuse before repairing either evidence store.
        let after = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.observe_source(&intent.request, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)??;
        after.verify_after(&intent.request)?;
        let ManagementEffectInspectOpenV1::Present(inspected) = self.inspect_management()? else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        if inspected.effects().any(|e| {
            e.request.operation_id != descriptor.management_operation_id
                && e.state.blocks_new_work()
        }) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let generic = inspected
            .effects()
            .find(|e| e.request.operation_id == descriptor.management_operation_id)
            .ok_or(ApplyOwnerError::EvidenceMismatch)?
            .clone();
        let binding = intent
            .d27_source_binding
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let joined = opened.join_marker_recovery_m2a(
            &precommit,
            descriptor,
            binding,
            (&intent.request, &intent.source_before),
            &generic.request,
            Some((&bytes, hash)),
        )?;
        let desktop_directory = opened.retained_root_directory_m2a()?;
        let recovery_directory = parent.retained_directory()?;
        let desktop_remaining = joined.remaining_publication_bytes(
            descriptor
                .capacity_plan
                .desktop_allocation_unit
                .ok_or(ApplyOwnerError::EvidenceMismatch)?,
        )?;
        let key = descriptor.operation_key;
        if matches!(generic.state, ManagementEffectStateV1::Completed { .. }) {
            self.require_live_capacity(
                &descriptor.capacity_plan,
                &recovery_directory,
                Some(&desktop_directory),
                Self::remaining_state_capacity(&intent, &generic.state, false)?,
                0,
                Some(desktop_remaining),
            )?;
            let predicted = joined.result_evidence_m2a(&precommit, hash)?;
            let marker_evidence = ApplyEvidenceV1::ParticipationCommitted {
                marker_kind: MarkerKindV1::D27Session,
                marker_hash: predicted.marker_evidence_hash,
                committed_revision: predicted.session_revision,
                sidecar_hash: Some(predicted.sidecar_hash),
                sidecar_bytes: Some(predicted.sidecar_offset),
                result_id_digest: Some(predicted.reserved_result_id_digest),
            };
            if !intent
                .generations
                .iter()
                .any(|e| e.generation.evidence == marker_evidence)
            {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            let expected = FinalReadbackV1 {
                schema_version: 1,
                descriptor_hash: descriptor.descriptor_hash()?,
                request_hash: intent.request.request_hash()?,
                source_observation_hash: after.observation_hash()?,
                journal_tip_hash: Some(tip),
                report_hash: Some(hash),
                marker_kind: Some(MarkerKindV1::D27Session),
                marker_hash: Some(predicted.marker_evidence_hash),
                agreed_revision: descriptor.agreed_revision.clone(),
            };
            // Certify the generic terminal body and retained marker before repair.
            let (store, completed_event) = if intent.latest().phase == ApplyPhaseV1::Completed {
                if !matches!(intent.latest().evidence, ApplyEvidenceV1::Completed { readback_hash, .. }
                    if readback_hash == expected.readback_hash()?)
                {
                    return Err(ApplyOwnerError::EvidenceMismatch);
                }
                (inspected.joined_terminal_store(&intent)?, None)
            } else {
                let (store, event) =
                    inspected.joined_completed_readback(&intent, expected.readback_hash()?)?;
                (store, Some(event))
            };
            let (mut writer, committed, _) = opened.recover_published_m2a(joined)?;
            if !committed {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            let readback = self.d27_committed_readback(&intent, parent, &mut writer, &precommit)?;
            let fresh = self.d27_committed_readback(&intent, parent, &mut writer, &precommit)?;
            if readback != expected || fresh != expected {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            if let Some(event) = completed_event {
                self.append_active(ApplyEvidenceV1::Completed {
                    readback_hash: fresh.readback_hash()?,
                    generic_generation: event.evidence.generation,
                    generic_event_hash: event.evidence.event_hash,
                    generic_sequence: event.evidence.sequence,
                })?;
            }
            self.archive_terminal_joined(Some(&store))?;
            return Ok((writer, ApplyOwnerRecoveryV1::Archived(key)));
        }
        if intent.latest().phase == ApplyPhaseV1::Completed {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        // All read-only joins precede generic or desktop publication repair.
        let token = self.join_management_recovery(*inspected)?;
        self.require_live_capacity(
            &descriptor.capacity_plan,
            &recovery_directory,
            Some(&desktop_directory),
            Self::remaining_state_capacity(
                &intent,
                &generic.state,
                token.requires_generation_seal()?,
            )?,
            0,
            Some(desktop_remaining),
        )?;
        let (store, _) = token.recover(&self.lease)?;
        let (mut writer, committed, _) = opened.recover_published_m2a(joined)?;
        if intent.latest().phase != ApplyPhaseV1::ApplyReported && !committed {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let mut operation = D27ApplyOperationV1 {
            owner: Some(self),
            desktop_root_directory: desktop_directory,
            desktop_result_remaining_bytes: 0,
            coordinates: (project.clone(), session.clone()),
            precommit,
            descriptor: descriptor.clone(),
            store,
            prepared: None,
            started: None,
            reserved: Some(child),
            reported: true,
            completed: false,
        };
        if !operation.save_result(&mut writer)? {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        Ok((writer, ApplyOwnerRecoveryV1::Archived(key)))
    }

    /// Certify saved historical success from the exact sealed archive and
    /// typed generic terminal event. Later source edits do not rewrite history.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn d27_result_is_completed(
        &self,
        published: &crate::desktop_store::Published,
        record: &crate::desktop_store::SavedSourceApply,
    ) -> Result<bool, ApplyOwnerError> {
        use crate::apply_intent::*;
        let Some(result) = &record.result else {
            return Ok(false);
        };
        let binding =
            ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes());
        let key = d27_operation_key(
            binding,
            d27_operation_binding(record.candidate.operation_id.as_str())?,
        )?;
        let Some(intent) = self.intents()?.archived(key)? else {
            return Ok(false);
        };
        if intent.descriptor().inventory_id != ApplyInventoryV1::D27
            || intent.descriptor().project_binding != binding
            || intent.latest().phase != ApplyPhaseV1::Completed
        {
            return Ok(false);
        }
        let precommit = Self::restore_d27_precommit(&intent, published)?;
        if result.result_id != precommit.reserved_result_id
            || record.reserved_result_id.as_ref() != Some(&result.result_id)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let report: crate::workspace_apply::ApplyReport =
            serde_json::from_value(result.report.clone())
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let hash = report
            .evidence_hash()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if report.failure.is_some() || !intent.generations.iter().any(|e| matches!(e.generation.evidence,
            ApplyEvidenceV1::ApplyReported { report_hash, result: ApplyResultV1::Success, .. } if report_hash == hash)) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let ManagementEffectInspectOpenV1::Present(inspected) = self.inspect_management()? else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        let snapshot = inspected
            .effects()
            .find(|e| e.request.operation_id == intent.descriptor().management_operation_id)
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let verified = inspected
            .latest_event_evidence(snapshot.request.operation_id, snapshot)?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        Self::verify_generic_join(&intent, snapshot, &verified)?;
        Ok(true)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn restore_d27_precommit(
        intent: &InspectedApplyIntentV1,
        published: &crate::desktop_store::Published,
    ) -> Result<crate::desktop_store::SourceApplyPrecommitV1, ApplyOwnerError> {
        use crate::apply_intent::*;
        let descriptor = intent.descriptor();
        let ApplyContextV1::Session {
            session_binding,
            base_epoch,
            base_session_revision,
            consume_target_revision,
            policy_revision,
            source_operation_digest,
            reserved_result_id,
            reserved_result_id_digest,
            base_marker_evidence_hash,
            consume_marker_evidence_hash,
            consume_sidecar_offset,
            consume_sidecar_hash,
            precommit_hash,
        } = &descriptor.context
        else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        if *session_binding != d27_session_binding(published.marker.session_id.as_str())?
            || *base_epoch != published.marker.epoch.get()
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let record = published
            .state
            .source_applies
            .iter()
            .find(|r| {
                d27_operation_binding(r.candidate.operation_id.as_str()).ok()
                    == Some(*source_operation_digest)
            })
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let result_id = polaris_desktop_protocol::ids::ResultId::new(reserved_result_id.clone())
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let mut consumed = record.clone();
        consumed.intent_revision = Some(polaris_desktop_protocol::ids::DecimalU64::new(
            *consume_target_revision,
        ));
        consumed.reserved_result_id = Some(result_id.clone());
        consumed.result = None;
        let mut bytes = b"polaris-d27-consume-record-v1\0".to_vec();
        bytes.extend(serde_json::to_vec(&consumed).map_err(|_| ApplyOwnerError::EvidenceMismatch)?);
        let precommit = crate::desktop_store::SourceApplyPrecommitV1 {
            schema_version: 1,
            project_binding: descriptor.project_binding,
            run_id: record.candidate.run_id.clone(),
            attempt_id: record.candidate.attempt_id.clone(),
            approval_id: record.candidate.approval_id.clone(),
            source_operation_digest: *source_operation_digest,
            payload_hash: ManagementEffectHashV1::from_hex(&descriptor.agreed_revision.value)
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            policy_revision: *policy_revision,
            base_session_revision: *base_session_revision,
            consume_target_revision: *consume_target_revision,
            reserved_result_id: result_id,
            reserved_result_id_digest: *reserved_result_id_digest,
            base_marker_evidence_hash: *base_marker_evidence_hash,
            consume_marker_evidence_hash: *consume_marker_evidence_hash,
            consume_sidecar_offset: *consume_sidecar_offset,
            consume_sidecar_hash: *consume_sidecar_hash,
            consume_record_hash: ManagementEffectHashV1::digest(&bytes),
            result_capacity_bytes: descriptor
                .capacity_plan
                .desktop_result_required_bytes
                .ok_or(ApplyOwnerError::EvidenceMismatch)?,
        };
        if precommit.precommit_hash()? != *precommit_hash {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        Ok(precommit)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn d27_committed_readback(
        &self,
        intent: &InspectedApplyIntentV1,
        parent: &crate::workspace_apply::RecoveryParent,
        writer: &mut crate::desktop_store::Writer,
        precommit: &crate::desktop_store::SourceApplyPrecommitV1,
    ) -> Result<crate::apply_intent::FinalReadbackV1, ApplyOwnerError> {
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.d27_committed_readback_protected(intent, parent, writer, precommit, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
    }

    fn d27_committed_readback_protected(
        &self,
        intent: &InspectedApplyIntentV1,
        parent: &crate::workspace_apply::RecoveryParent,
        writer: &mut crate::desktop_store::Writer,
        precommit: &crate::desktop_store::SourceApplyPrecommitV1,
        secrets: &crate::isolated_workspace::RegisteredSecrets,
    ) -> Result<crate::apply_intent::FinalReadbackV1, ApplyOwnerError> {
        use crate::apply_intent::*;
        let descriptor = intent.descriptor();
        let child = crate::workspace_apply::inspect_reserved_recovery_v2(
            parent,
            descriptor.operation_key,
            descriptor.descriptor_hash()?,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
        .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        self.verify_recovery_child(&child, intent)?;
        let bytes = read_report_bytes(child.directory(), descriptor.capacity_plan.report_bytes)?;
        let hash = read_report_hash(child.directory(), descriptor.capacity_plan.report_bytes)?;
        let journal = child
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let tip = crate::workspace_apply::journal_tip_hash(
            &journal,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if !intent.generations.iter().any(|e| matches!(e.generation.evidence,
            ApplyEvidenceV1::ApplyReported { report_hash, journal_tip_hash, result: ApplyResultV1::Success, .. }
                if report_hash == hash && journal_tip_hash == tip))
            || !writer.snapshot()?.state.source_applies.iter().any(|r|
                r.candidate.run_id == precommit.run_id && r.candidate.attempt_id == precommit.attempt_id
                && r.candidate.approval_id == precommit.approval_id && r.result.is_some()) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let result = writer.record_source_apply_result_evidence_m2a(
            precommit,
            descriptor,
            precommit.reserved_result_id.clone(),
            &bytes,
            hash,
        )?;
        let expected = ApplyEvidenceV1::ParticipationCommitted {
            marker_kind: MarkerKindV1::D27Session,
            marker_hash: result.marker_evidence_hash,
            committed_revision: result.session_revision,
            sidecar_hash: Some(result.sidecar_hash),
            sidecar_bytes: Some(result.sidecar_offset),
            result_id_digest: Some(result.reserved_result_id_digest),
        };
        if !intent
            .generations
            .iter()
            .any(|e| e.generation.evidence == expected)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let after = self.observe_source(&intent.request, secrets)?;
        after.verify_after(&intent.request)?;
        Ok(FinalReadbackV1 {
            schema_version: 1,
            descriptor_hash: descriptor.descriptor_hash()?,
            request_hash: intent.request.request_hash()?,
            source_observation_hash: after.observation_hash()?,
            journal_tip_hash: Some(tip),
            report_hash: Some(hash),
            marker_kind: Some(MarkerKindV1::D27Session),
            marker_hash: Some(result.marker_evidence_hash),
            agreed_revision: descriptor.agreed_revision.clone(),
        })
    }

    fn c12_committed_readback(
        &self,
        intent: &InspectedApplyIntentV1,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<crate::apply_intent::FinalReadbackV1, ApplyOwnerError> {
        use crate::apply_intent::*;
        use crate::sync::participation::*;
        let descriptor = intent.descriptor();
        let ApplyContextV1::Participation {
            target_generation,
            target_marker_hash,
            target_participation_hash,
            ..
        } = descriptor.context
        else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        if parent.identity()
            != (
                descriptor.recovery_parent_identity.device,
                descriptor.recovery_parent_identity.inode,
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let child = crate::workspace_apply::inspect_reserved_recovery_v2(
            parent,
            descriptor.operation_key,
            descriptor.descriptor_hash()?,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
        .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        self.verify_recovery_child(&child, intent)?;
        let journal = child
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let tip = crate::workspace_apply::journal_tip_hash(
            &journal,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let report_hash =
            read_report_hash(child.directory(), descriptor.capacity_plan.report_bytes)?;
        let inventory = C12GenerationOwnerV1::open(&self.state_dir, &self.lease)?;
        let C12InspectionV1::Committed {
            marker,
            pending: None,
        } = inventory.inspect()?
        else {
            return Err(ApplyOwnerError::EvidenceMismatch);
        };
        let body = inventory
            .current_body()?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let snapshot = decode_participation_v2(&body)?;
        let participation = snapshot
            .participation()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if marker.generation != target_generation
            || marker.mutation_id != Some(descriptor.operation_key)
            || marker.marker_hash()? != target_marker_hash
            || marker.participation_hash != target_participation_hash
            || marker.publication != ParticipationPublicationV1::Apply
            || Some(participation_bytes_hash(&body)) != target_participation_hash
            || participation.applied_commit.as_deref()
                != Some(descriptor.agreed_revision.value.as_str())
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let report_record = intent
            .generations
            .iter()
            .find_map(|e| match e.generation.evidence {
                ApplyEvidenceV1::ApplyReported {
                    journal_tip_hash,
                    report_hash,
                    result: ApplyResultV1::Success,
                    ..
                } => Some((journal_tip_hash, report_hash)),
                _ => None,
            });
        let marker_record = intent
            .generations
            .iter()
            .find_map(|e| match e.generation.evidence {
                ApplyEvidenceV1::ParticipationCommitted {
                    marker_kind: MarkerKindV1::C12Generation,
                    marker_hash,
                    committed_revision,
                    sidecar_hash,
                    sidecar_bytes,
                    result_id_digest: None,
                } => Some((marker_hash, committed_revision, sidecar_hash, sidecar_bytes)),
                _ => None,
            });
        if report_record != Some((tip, report_hash))
            || marker_record
                != Some((
                    target_marker_hash,
                    target_generation,
                    target_participation_hash,
                    Some(body.len() as u64),
                ))
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let after = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.observe_source(&intent.request, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)??;
        after.verify_after(&intent.request)?;
        Ok(FinalReadbackV1 {
            schema_version: 1,
            descriptor_hash: descriptor.descriptor_hash()?,
            request_hash: intent.request.request_hash()?,
            source_observation_hash: after.observation_hash()?,
            journal_tip_hash: Some(tip),
            report_hash: Some(report_hash),
            marker_kind: Some(MarkerKindV1::C12Generation),
            marker_hash: Some(target_marker_hash),
            agreed_revision: descriptor.agreed_revision.clone(),
        })
    }

    pub fn join_management_recovery(
        &self,
        inspected: ManagementEffectInspectV1,
    ) -> Result<JoinedManagementEffectRecoveryV1, ApplyOwnerError> {
        let mut intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let project =
            ManagementEffectHashV1::digest(self.project_root.as_os_str().as_encoded_bytes());
        if intent.descriptor().project_binding != project {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let operation_id = intent.descriptor().management_operation_id;
        if inspected.effects().any(|effect| {
            effect.request.operation_id != operation_id && effect.state.blocks_new_work()
        }) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if intent.latest().phase == crate::apply_intent::ApplyPhaseV1::RecoveryPrepared {
            let request = Self::management_request(intent.descriptor())?;
            let snapshot = inspected
                .effects()
                .find(|snapshot| snapshot.request.operation_id == operation_id)
                .ok_or(ApplyOwnerError::EvidenceMismatch)?;
            if snapshot.request != request {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            let verified = inspected
                .latest_event_evidence(operation_id, snapshot)?
                .ok_or(ApplyOwnerError::EvidenceMismatch)?;
            if matches!(
                verified.event,
                ManagementEffectEventV1::EffectStarted { .. }
            ) {
                let (generation, event_hash, sequence) = intent
                    .latest()
                    .evidence
                    .generic_evidence()
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?;
                inspected
                    .event_evidence(
                        operation_id,
                        ManagementEffectEventEvidenceV1 {
                            generation,
                            event_hash,
                            sequence,
                        },
                        &ManagementEffectEventV1::Prepared { request },
                    )?
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?;
                self.append_evidence(
                    &intent,
                    ApplyEvidenceV1::EffectStarted {
                        generic_generation: verified.evidence.generation,
                        generic_event_hash: verified.evidence.event_hash,
                        generic_sequence: verified.evidence.sequence,
                    },
                )?;
                intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
            }
        }
        inspected.join_recovery(&intent)
    }

    fn append_evidence(
        &self,
        intent: &InspectedApplyIntentV1,
        evidence: ApplyEvidenceV1,
    ) -> Result<(), ApplyOwnerError> {
        if intent.latest().evidence == evidence {
            return Ok(());
        }
        let generation = ApplyGenerationV1 {
            schema_version: 1,
            sequence: intent
                .latest()
                .sequence
                .checked_add(1)
                .ok_or(ApplyOwnerError::EvidenceMismatch)?,
            previous_record_hash: intent.latest().record_hash()?,
            descriptor_hash: intent.descriptor().descriptor_hash()?,
            phase: evidence.phase(),
            evidence,
        };
        self.intents()?
            .append(intent.descriptor().operation_key, generation)?;
        Ok(())
    }

    /// Never replays a source effect. A started effect is marked unknown;
    /// prepared recovery verifies unchanged source and retains a header child.
    pub fn recover_joined_interrupted(
        &self,
        token: JoinedManagementEffectRecoveryV1,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<VerifiedManagementEffectEventV1, ApplyOwnerError> {
        Ok(self.recover_joined_store(token, parent)?.1)
    }

    fn recover_joined_store(
        &self,
        token: JoinedManagementEffectRecoveryV1,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<(ManagementEffectStoreV1, VerifiedManagementEffectEventV1), ApplyOwnerError> {
        let intent = self.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if token.intent_binding()
            != (
                intent.descriptor().descriptor_hash()?,
                intent.latest().sequence,
                intent.latest().record_hash()?,
            )
            || intent.latest().phase.is_terminal()
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let disposition = if matches!(
            token.generic_event().event,
            ManagementEffectEventV1::Prepared { .. } | ManagementEffectEventV1::Denied { .. }
        ) {
            self.verify_source_before(&intent)?;
            Some(self.inspect_header_child(&intent, parent)?)
        } else {
            None
        };
        if intent.descriptor().inventory_id == crate::apply_intent::ApplyInventoryV1::C12 {
            let state = match token.generic_event().event {
                ManagementEffectEventV1::Prepared { .. } => ManagementEffectStateV1::Prepared,
                ManagementEffectEventV1::EffectStarted { .. } => {
                    ManagementEffectStateV1::EffectStarted
                }
                ManagementEffectEventV1::OutcomeUnknown { reason, .. } => {
                    ManagementEffectStateV1::OutcomeUnknown { reason }
                }
                _ => ManagementEffectStateV1::Denied {
                    reason: ManagementDenialReasonV1::InvalidState,
                },
            };
            self.require_live_capacity(
                &intent.descriptor().capacity_plan,
                &parent.retained_directory()?,
                None,
                Self::remaining_state_capacity(&intent, &state, token.requires_generation_seal()?)?,
                0,
                None,
            )?;
        }
        let (store, mut event) = token.recover(&self.lease)?;
        if matches!(event.event, ManagementEffectEventV1::Prepared { .. }) {
            let prepared = store
                .prepared_effect(intent.descriptor().management_operation_id)?
                .ok_or(ApplyOwnerError::EvidenceMismatch)?;
            event = store
                .deny(
                    &self.lease,
                    &prepared,
                    ManagementDenialReasonV1::InvalidState,
                )?
                .verified_event()
                .clone();
        }
        let evidence = match event.event {
            ManagementEffectEventV1::Denied { reason, .. } => {
                ApplyEvidenceV1::AbandonedBeforeEffect {
                    source_readback_hash: intent.source_before.observation_hash()?,
                    child_disposition: disposition.ok_or(ApplyOwnerError::EvidenceMismatch)?,
                    generic_denial_reason: Some(reason),
                    generic_generation: Some(event.evidence.generation),
                    generic_event_hash: Some(event.evidence.event_hash),
                    generic_sequence: Some(event.evidence.sequence),
                }
            }
            ManagementEffectEventV1::OutcomeUnknown { reason, .. } => {
                let mut journal_tip_hash = None;
                let mut report_hash = None;
                let mut marker_hash = None;
                for record in &intent.generations {
                    match record.generation.evidence {
                        ApplyEvidenceV1::ApplyReported {
                            report_hash: report,
                            journal_tip_hash: tip,
                            ..
                        } => {
                            journal_tip_hash = Some(tip);
                            report_hash = Some(report);
                        }
                        ApplyEvidenceV1::ParticipationCommitted {
                            marker_hash: marker,
                            ..
                        } => marker_hash = Some(marker),
                        ApplyEvidenceV1::OutcomeUnknown {
                            journal_tip_hash: tip,
                            report_hash: report,
                            marker_hash: marker,
                            ..
                        } => {
                            journal_tip_hash = tip;
                            report_hash = report;
                            marker_hash = marker;
                        }
                        _ => {}
                    }
                }
                ApplyEvidenceV1::OutcomeUnknown {
                    reason,
                    journal_tip_hash,
                    report_hash,
                    marker_hash,
                    generic_generation: event.evidence.generation,
                    generic_event_hash: event.evidence.event_hash,
                    generic_sequence: event.evidence.sequence,
                }
            }
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        };
        self.append_evidence(&intent, evidence)?;
        Ok((store, event))
    }

    fn inspect_header_child(
        &self,
        intent: &InspectedApplyIntentV1,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<crate::apply_intent::ChildDispositionV1, ApplyOwnerError> {
        use crate::apply_intent::ChildDispositionV1;
        let descriptor = intent.descriptor();
        if parent.identity()
            != (
                descriptor.recovery_parent_identity.device,
                descriptor.recovery_parent_identity.inode,
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if let Some(binding) = &intent.d27_source_binding
            && parent.provenance() != Path::new(&binding.manifest.recovery_parent_path)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let child = crate::workspace_apply::inspect_reserved_recovery_v2(
            parent,
            descriptor.operation_key,
            descriptor.descriptor_hash()?,
            descriptor.capacity_plan.journal_bytes,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let Some(child) = child else {
            if intent.latest().phase != crate::apply_intent::ApplyPhaseV1::Prepared
                && !matches!(
                    intent.latest().evidence,
                    ApplyEvidenceV1::AbandonedBeforeEffect {
                        child_disposition: ChildDispositionV1::Absent,
                        ..
                    }
                )
            {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            return Ok(ChildDispositionV1::Absent);
        };
        let bytes = child
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let journal =
            crate::workspace_apply::decode_journal(&bytes, descriptor.capacity_plan.journal_bytes)
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if !journal.entries.is_empty() || child.header().source != self.project_root {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if crate::project_mutation::directory_names_bounded(child.directory(), 1)?
            != [std::ffi::OsString::from("journal.jsonl")]
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        self.verify_recovery_child(&child, intent)?;
        Ok(ChildDispositionV1::RetainedHeaderOnly)
    }

    fn verify_recovery_child(
        &self,
        child: &crate::workspace_apply::ReservedRecoveryV2,
        intent: &InspectedApplyIntentV1,
    ) -> Result<(), ApplyOwnerError> {
        if child.header().source != self.project_root {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let header = child.header();
        if header.source_chain.last().map(|entry| entry.1)
            != Some((
                intent.request.source_identity.device,
                intent.request.source_identity.inode,
            ))
            || header.entries.len() != intent.request.entries.len()
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let hex = |hash: &[u8; 32]| {
            hash.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        for (header, request) in header.entries.iter().zip(&intent.request.entries) {
            if header.path != Path::new(&request.path) {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            match (&header.before, &request.before) {
                (None, None) => {}
                (Some(header), Some(request))
                    if (header.device, header.inode, header.bytes, header.mode)
                        == (
                            request.device,
                            request.inode,
                            request.bytes,
                            request.observed_mode,
                        )
                        && hex(&header.hash) == request.hash.as_hex() => {}
                _ => return Err(ApplyOwnerError::EvidenceMismatch),
            }
            match (&header.after, &request.after) {
                (None, None) => {}
                (Some(header), Some(request))
                    if (header.bytes, header.mode) == (request.bytes, request.mode)
                        && hex(&header.hash) == request.hash.as_hex() => {}
                _ => return Err(ApplyOwnerError::EvidenceMismatch),
            }
        }
        let header_record =
            intent
                .generations
                .iter()
                .find_map(|record| match record.generation.evidence {
                    ApplyEvidenceV1::RecoveryPrepared {
                        journal_header_hash,
                        journal_header_bytes,
                        child_identity,
                        directory_synced: true,
                        ..
                    } => Some((journal_header_hash, journal_header_bytes, child_identity)),
                    _ => None,
                });
        use std::os::unix::fs::MetadataExt;
        let meta = child.directory().metadata()?;
        let actual = (
            child
                .header()
                .header_hash()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            child
                .header()
                .canonical_bytes()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
                .len() as u64,
            crate::apply_intent::IdentityV1 {
                device: meta.dev(),
                inode: meta.ino(),
            },
        );
        if header_record.is_some_and(|record| record != actual)
            || (header_record.is_none()
                && intent.latest().phase != crate::apply_intent::ApplyPhaseV1::Prepared)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if header.source_chain != directory_chain(&self.project_root)? {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let mut recovery_chain = directory_chain(
            child
                .path()
                .parent()
                .ok_or(ApplyOwnerError::EvidenceMismatch)?,
        )?;
        recovery_chain.push((child.path().to_owned(), (meta.dev(), meta.ino())));
        if header.recovery_chain != recovery_chain {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        Ok(())
    }

    fn verify_source_before(&self, intent: &InspectedApplyIntentV1) -> Result<(), ApplyOwnerError> {
        intent.source_before.verify_before(&intent.request)?;
        let observed = polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.observe_source(&intent.request, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)??;
        if observed != intent.source_before {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        Ok(())
    }

    fn observe_source(
        &self,
        request: &crate::apply_intent::RequestV1,
        secrets: &crate::isolated_workspace::RegisteredSecrets,
    ) -> Result<crate::apply_intent::ObservedSourceV1, ApplyOwnerError> {
        use crate::apply_intent::{BeforeV1, ObservedEntryV1, ObservedSourceV1};
        use crate::project_mutation::open_directory_nofollow;
        use sha2::{Digest, Sha256};
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
            os::unix::fs::{FileExt, MetadataExt},
        };
        request.canonical_bytes()?;
        let root = open_directory_nofollow(&self.project_root)?;
        let root_meta = root.metadata()?;
        if (root_meta.dev(), root_meta.ino())
            != (
                request.source_identity.device,
                request.source_identity.inode,
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let mut entries = Vec::with_capacity(request.entries.len());
        for entry in &request.entries {
            let absolute = self.project_root.join(&entry.path);
            if secrets.reserves(&absolute) {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            let parent_path = absolute.parent().ok_or(ApplyOwnerError::EvidenceMismatch)?;
            let parent = open_directory_nofollow(parent_path)?;
            let name = CString::new(
                absolute
                    .file_name()
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?
                    .as_encoded_bytes(),
            )
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            let current = if fd < 0 {
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
                    return Err(ApplyOwnerError::EvidenceMismatch);
                }
                None
            } else {
                let file = unsafe { std::fs::File::from_raw_fd(fd) };
                let meta = file.metadata()?;
                if !meta.is_file() || meta.nlink() != 1 || secrets.denies(&absolute, &meta) {
                    return Err(ApplyOwnerError::EvidenceMismatch);
                }
                // The approved request bounds this read even if the file grew.
                let maximum = entry
                    .before
                    .as_ref()
                    .map_or(0, |v| v.bytes)
                    .max(entry.after.as_ref().map_or(0, |v| v.bytes));
                if meta.len() > maximum {
                    return Err(ApplyOwnerError::EvidenceMismatch);
                }
                let mut digest = Sha256::new();
                let mut buffer = [0u8; 65536];
                let mut offset = 0;
                while offset < meta.len() {
                    let count = (meta.len() - offset).min(buffer.len() as u64) as usize;
                    file.read_exact_at(&mut buffer[..count], offset)?;
                    digest.update(&buffer[..count]);
                    offset += count as u64;
                }
                let now = file.metadata()?;
                let attached = std::fs::symlink_metadata(&absolute)?;
                if (attached.dev(), attached.ino()) != (meta.dev(), meta.ino())
                    || (
                        now.dev(),
                        now.ino(),
                        now.len(),
                        now.mode(),
                        now.mtime(),
                        now.mtime_nsec(),
                        now.ctime(),
                        now.ctime_nsec(),
                    ) != (
                        meta.dev(),
                        meta.ino(),
                        meta.len(),
                        meta.mode(),
                        meta.mtime(),
                        meta.mtime_nsec(),
                        meta.ctime(),
                        meta.ctime_nsec(),
                    )
                {
                    return Err(ApplyOwnerError::EvidenceMismatch);
                }
                Some(BeforeV1 {
                    device: meta.dev(),
                    inode: meta.ino(),
                    bytes: meta.len(),
                    mode: meta.mode() & 0o777,
                    observed_mode: meta.mode() & 0o7777,
                    hash: ManagementEffectHashV1::from_hex(&format!("{:x}", digest.finalize()))
                        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
                })
            };
            let attached_parent = open_directory_nofollow(parent_path)?.metadata()?;
            let pinned_parent = parent.metadata()?;
            if (attached_parent.dev(), attached_parent.ino())
                != (pinned_parent.dev(), pinned_parent.ino())
            {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            entries.push(ObservedEntryV1 {
                path: entry.path.clone(),
                current,
            });
        }
        let attached = open_directory_nofollow(&self.project_root)?.metadata()?;
        if (attached.dev(), attached.ino()) != (root_meta.dev(), root_meta.ino()) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let observed = ObservedSourceV1 {
            schema_version: 1,
            source_identity: request.source_identity,
            entries,
        };
        observed.canonical_bytes()?;
        Ok(observed)
    }

    /// Mapping is exact; durable recovery artifacts do not change inventory
    /// rewind semantics or replace the descriptor hash with a workspace hash.
    pub fn management_request(
        descriptor: &ApplyIntentDescriptorV1,
    ) -> Result<PrepareManagementEffectV1, ApplyOwnerError> {
        descriptor.canonical_bytes()?;
        let (inventory, session, generation, revision) =
            match (&descriptor.inventory_id, &descriptor.context) {
                (
                    ApplyInventoryV1::C12,
                    ApplyContextV1::Participation {
                        base_generation,
                        target_generation,
                        ..
                    },
                ) => ("C12", None, *base_generation, *target_generation),
                (
                    ApplyInventoryV1::D27,
                    ApplyContextV1::Session {
                        session_binding,
                        base_epoch,
                        consume_target_revision,
                        ..
                    },
                ) => (
                    "D27",
                    Some(*session_binding),
                    *base_epoch,
                    *consume_target_revision,
                ),
                _ => return Err(ApplyOwnerError::EvidenceMismatch),
            };
        Ok(PrepareManagementEffectV1 {
            operation_id: descriptor.management_operation_id,
            inventory: ManagementInventoryIdV1::parse(inventory).expect("fixed inventory"),
            scope: ManagementEffectScopeV1 {
                project: descriptor.project_binding,
                session,
                generation,
                revision,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: descriptor.descriptor_hash()?,
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        })
    }

    /// A real store lookup must supply the verified event. This compares the
    /// full request, phase, body and original envelope position independently.
    pub(crate) fn verify_generic_join(
        intent: &InspectedApplyIntentV1,
        snapshot: &ManagementEffectSnapshotV1,
        verified: &VerifiedManagementEffectEventV1,
    ) -> Result<(), ApplyOwnerError> {
        let expected_request = Self::management_request(intent.descriptor())?;
        if snapshot.request != expected_request
            || verified.event.operation_id() != expected_request.operation_id
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if intent.latest().phase == crate::apply_intent::ApplyPhaseV1::Prepared {
            if verified.event
                == (ManagementEffectEventV1::Prepared {
                    request: expected_request,
                })
                && snapshot.state == ManagementEffectStateV1::Prepared
                && snapshot.last_sequence == verified.evidence.sequence
            {
                return Ok(());
            }
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let evidence = intent
            .generations
            .iter()
            .rev()
            .find_map(|record| {
                record
                    .generation
                    .evidence
                    .generic_evidence()
                    .map(|triple| (&record.generation.evidence, triple))
            })
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if evidence.1
            != (
                verified.evidence.generation,
                verified.evidence.event_hash,
                verified.evidence.sequence,
            )
            || snapshot.last_sequence != verified.evidence.sequence
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let operation_id = expected_request.operation_id;
        let (event, state) = match evidence.0 {
            ApplyEvidenceV1::RecoveryPrepared { .. } => (
                ManagementEffectEventV1::Prepared {
                    request: expected_request,
                },
                ManagementEffectStateV1::Prepared,
            ),
            ApplyEvidenceV1::EffectStarted { .. } => (
                ManagementEffectEventV1::EffectStarted { operation_id },
                ManagementEffectStateV1::EffectStarted,
            ),
            ApplyEvidenceV1::Denied { reason, .. } => (
                ManagementEffectEventV1::Denied {
                    operation_id,
                    reason: *reason,
                },
                ManagementEffectStateV1::Denied { reason: *reason },
            ),
            ApplyEvidenceV1::AbandonedBeforeEffect {
                generic_denial_reason: Some(reason),
                ..
            } => (
                ManagementEffectEventV1::Denied {
                    operation_id,
                    reason: *reason,
                },
                ManagementEffectStateV1::Denied { reason: *reason },
            ),
            ApplyEvidenceV1::OutcomeUnknown { reason, .. } => (
                ManagementEffectEventV1::OutcomeUnknown {
                    operation_id,
                    reason: *reason,
                },
                ManagementEffectStateV1::OutcomeUnknown { reason: *reason },
            ),
            ApplyEvidenceV1::Completed { readback_hash, .. } => (
                ManagementEffectEventV1::Completed {
                    operation_id,
                    evidence_hash: *readback_hash,
                },
                ManagementEffectStateV1::Completed {
                    evidence_hash: *readback_hash,
                },
            ),
            ApplyEvidenceV1::ReconciledAbandon { readback_hash, .. } => (
                ManagementEffectEventV1::ReconciledAbandon {
                    operation_id,
                    evidence_hash: *readback_hash,
                },
                ManagementEffectStateV1::ReconciledAbandon {
                    evidence_hash: *readback_hash,
                },
            ),
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        };
        if verified.event != event || snapshot.state != state {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl D27ApplyOperationV1 {
    pub fn is_completed(&self) -> bool {
        self.completed
    }

    pub fn apply(
        &mut self,
        snapshot: &crate::isolated_workspace::Snapshot,
        changes: &crate::isolated_workspace::ChangeSet,
        limits: crate::isolated_workspace::Limits,
        parent: &crate::workspace_apply::RecoveryParent,
    ) -> Result<crate::workspace_apply::ApplyReport, ApplyOwnerError> {
        // One attempt even when an unwind interrupts preparation or dispatch.
        if self.reported || self.started.is_some() || self.completed {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.apply_protected(snapshot, changes, limits, parent, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
    }

    fn apply_protected(
        &mut self,
        snapshot: &crate::isolated_workspace::Snapshot,
        changes: &crate::isolated_workspace::ChangeSet,
        limits: crate::isolated_workspace::Limits,
        parent: &crate::workspace_apply::RecoveryParent,
        secrets: &crate::isolated_workspace::RegisteredSecrets,
    ) -> Result<crate::workspace_apply::ApplyReport, ApplyOwnerError> {
        use crate::apply_intent::*;
        use crate::workspace_apply as workspace;
        use std::os::unix::fs::MetadataExt;
        let owner = self
            .owner
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let intent = owner.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let descriptor = intent.descriptor();
        if intent.latest().phase != ApplyPhaseV1::Prepared
            || descriptor.inventory_id != ApplyInventoryV1::D27
            || Self::precommit_hash(&intent)? != self.precommit.precommit_hash()?
            || snapshot.source_path != owner.project_root
            || request_from_changes(snapshot, changes)? != intent.request
            || owner.observe_source(&intent.request, secrets)? != intent.source_before
            || parent.identity()
                != (
                    descriptor.recovery_parent_identity.device,
                    descriptor.recovery_parent_identity.inode,
                )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let recovery_directory = parent.retained_directory()?;
        owner
            .require_live_capacity(
                &descriptor.capacity_plan,
                &recovery_directory,
                Some(&self.desktop_root_directory),
                ApplyOwnerV1::remaining_state_capacity(
                    &intent,
                    &ManagementEffectStateV1::Prepared,
                    false,
                )?,
                descriptor.capacity_plan.recovery_required_bytes,
                Some(self.desktop_result_remaining_bytes),
            )
            .map_err(|error| match error {
                ApplyOwnerError::Intent(ApplyIntentError::Capacity(_)) => {
                    ApplyOwnerError::ResourceUnavailableBeforeEffect
                }
                other => other,
            })?;
        // Only a proved capacity preflight refusal may retain this attempt.
        // All preparation/dispatch failures from here remain non-replayable.
        self.reported = true;
        let reserved = workspace::prepare_reserved_recovery_v2(
            snapshot,
            changes,
            secrets,
            limits,
            parent,
            descriptor.operation_key,
            descriptor.descriptor_hash()?,
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let child = reserved.directory().metadata()?;
        let prepared = self
            .prepared
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let proof = prepared.verified_event().evidence;
        owner.append_active(ApplyEvidenceV1::RecoveryPrepared {
            child_identity: IdentityV1 {
                device: child.dev(),
                inode: child.ino(),
            },
            journal_header_hash: reserved
                .header()
                .header_hash()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?,
            journal_header_bytes: reserved
                .header()
                .canonical_bytes()
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
                .len() as u64,
            directory_synced: true,
            generic_prepared_generation: proof.generation,
            generic_prepared_hash: proof.event_hash,
            generic_prepared_sequence: proof.sequence,
        })?;
        self.reserved = Some(reserved);
        self.started = Some(self.store.start(&owner.lease, prepared)?);
        let started = self.started.as_ref().expect("started capability retained");
        let proof = started.verified_event().evidence;
        owner.append_active(ApplyEvidenceV1::EffectStarted {
            generic_generation: proof.generation,
            generic_event_hash: proof.event_hash,
            generic_sequence: proof.sequence,
        })?;
        let reserved = self.reserved.as_ref().expect("recovery retained");
        let report = workspace::apply_reserved_v2(snapshot, changes, secrets, limits, reserved);
        let report_hash = report
            .publish_exact(reserved.directory())
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let report_bytes = report
            .canonical_bytes()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
            .len() as u64;
        let journal = reserved
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let tip = workspace::journal_tip_hash(&journal, descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let success =
            report.failure.is_none()
                && report.entries.len() == intent.request.entries.len()
                && report.entries.iter().zip(&intent.request.entries).all(
                    |(reported, requested)| {
                        reported.relative_path == Path::new(&requested.path)
                            && if requested.after.is_some() {
                                reported.installed
                            } else {
                                reported.deleted
                            }
                    },
                );
        let result = if success {
            ApplyResultV1::Success
        } else if report
            .entries
            .iter()
            .any(|e| e.installed || e.deleted || e.old_retained)
        {
            ApplyResultV1::Partial
        } else {
            ApplyResultV1::Failed
        };
        owner.append_active(ApplyEvidenceV1::ApplyReported {
            report_hash,
            report_bytes,
            journal_tip_hash: tip,
            result,
        })?;
        #[cfg(any(test, feature = "execution-test-support"))]
        owner_stop_after(ApplyPhaseV1::ApplyReported)?;
        if !success {
            let unknown = self.store.mark_outcome_unknown(
                &owner.lease,
                started,
                ManagementUnknownReasonV1::UnclassifiedEffect,
            )?;
            let proof = unknown.verified_event().evidence;
            owner.append_active(ApplyEvidenceV1::OutcomeUnknown {
                reason: ManagementUnknownReasonV1::UnclassifiedEffect,
                journal_tip_hash: Some(tip),
                report_hash: Some(report_hash),
                marker_hash: None,
                generic_generation: proof.generation,
                generic_event_hash: proof.event_hash,
                generic_sequence: proof.sequence,
            })?;
        }
        Ok(report)
    }

    fn precommit_hash(
        intent: &InspectedApplyIntentV1,
    ) -> Result<ManagementEffectHashV1, ApplyOwnerError> {
        match intent.descriptor().context {
            ApplyContextV1::Session { precommit_hash, .. } => Ok(precommit_hash),
            _ => Err(ApplyOwnerError::EvidenceMismatch),
        }
    }

    /// Only terminal readback may release the lease or produce success. False
    /// retains the unknown operation and all recovery ownership without retry.
    pub fn save_result(
        &mut self,
        writer: &mut crate::desktop_store::Writer,
    ) -> Result<bool, ApplyOwnerError> {
        if writer.coordinates() != (&self.coordinates.0, &self.coordinates.1) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if self.completed {
            self.verify_completed_result(writer)?;
            return Ok(true);
        }
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            self.save_result_protected(writer, &secrets)
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
    }

    fn verify_completed_result(
        &self,
        writer: &mut crate::desktop_store::Writer,
    ) -> Result<(), ApplyOwnerError> {
        let reserved = self
            .reserved
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let bytes = read_report_bytes(
            reserved.directory(),
            self.descriptor.capacity_plan.report_bytes,
        )?;
        let hash = read_report_hash(
            reserved.directory(),
            self.descriptor.capacity_plan.report_bytes,
        )?;
        let saved = writer.snapshot()?;
        if !saved.state.source_applies.iter().any(|r| {
            r.candidate.run_id == self.precommit.run_id
                && r.candidate.attempt_id == self.precommit.attempt_id
                && r.candidate.approval_id == self.precommit.approval_id
                && r.result
                    .as_ref()
                    .is_some_and(|v| v.result_id == self.precommit.reserved_result_id)
        }) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        writer.record_source_apply_result_evidence_m2a(
            &self.precommit,
            &self.descriptor,
            self.precommit.reserved_result_id.clone(),
            &bytes,
            hash,
        )?;
        Ok(())
    }

    /// Explicit result-only repair. It joins the exact retained report and
    /// typed generic event before repairing marker publication; never apply.
    pub fn recover_result_save(
        &mut self,
        writer: &mut crate::desktop_store::Writer,
    ) -> Result<(), ApplyOwnerError> {
        if writer.coordinates() != (&self.coordinates.0, &self.coordinates.1) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if self.completed {
            return self.verify_completed_result(writer);
        }
        let owner = self
            .owner
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let intent = owner.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if Self::precommit_hash(&intent)? != self.precommit.precommit_hash()?
            || !matches!(
                intent.latest().phase,
                crate::apply_intent::ApplyPhaseV1::ApplyReported
                    | crate::apply_intent::ApplyPhaseV1::ParticipationCommitted
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let snapshot = self
            .store
            .effect(intent.descriptor().management_operation_id)?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let verified = self
            .store
            .latest_event_evidence(snapshot.request.operation_id, &snapshot)?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        ApplyOwnerV1::verify_generic_join(&intent, &snapshot, &verified)?;
        let reserved = self
            .reserved
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let report_bytes = read_report_bytes(
            reserved.directory(),
            intent.descriptor().capacity_plan.report_bytes,
        )?;
        let hash = read_report_hash(
            reserved.directory(),
            intent.descriptor().capacity_plan.report_bytes,
        )?;
        let binding = intent
            .d27_source_binding
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
                    .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            owner
                .observe_source(&intent.request, &secrets)?
                .verify_after(&intent.request)?;
            Ok::<(), ApplyOwnerError>(())
        })
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)??;
        let opened = writer.inspect_retained_recovery_m2a(intent.descriptor().project_binding)?;
        let joined = opened.join_marker_recovery_m2a(
            &self.precommit,
            intent.descriptor(),
            binding,
            (&intent.request, &intent.source_before),
            &snapshot.request,
            Some((&report_bytes, hash)),
        )?;
        let desktop_directory = opened.retained_root_directory_m2a()?;
        let recovery_directory = reserved.retained_parent_directory()?;
        owner.require_live_capacity(
            &intent.descriptor().capacity_plan,
            &recovery_directory,
            Some(&desktop_directory),
            ApplyOwnerV1::remaining_state_capacity(&intent, &snapshot.state, false)?,
            0,
            Some(
                joined.remaining_publication_bytes(
                    intent
                        .descriptor()
                        .capacity_plan
                        .desktop_allocation_unit
                        .ok_or(ApplyOwnerError::EvidenceMismatch)?,
                )?,
            ),
        )?;
        let (recovered, _, _) = opened.recover_published_m2a(joined)?;
        *writer = recovered;
        Ok(())
    }

    fn save_result_protected(
        &mut self,
        writer: &mut crate::desktop_store::Writer,
        secrets: &crate::isolated_workspace::RegisteredSecrets,
    ) -> Result<bool, ApplyOwnerError> {
        use crate::apply_intent::*;
        use crate::workspace_apply as workspace;
        let owner = self
            .owner
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let intent = owner.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if Self::precommit_hash(&intent)? != self.precommit.precommit_hash()? {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let committed = intent
            .generations
            .iter()
            .find_map(|e| match &e.generation.evidence {
                e @ ApplyEvidenceV1::ParticipationCommitted { .. } => Some(e.clone()),
                _ => None,
            });
        if intent.latest().phase == ApplyPhaseV1::OutcomeUnknown
            && (self.started.is_some() || committed.is_none())
        {
            return Ok(false);
        }
        let descriptor = intent.descriptor();
        let reserved = self
            .reserved
            .as_ref()
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let journal = reserved
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let tip = workspace::journal_tip_hash(&journal, descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let report_bytes =
            read_report_bytes(reserved.directory(), descriptor.capacity_plan.report_bytes)?;
        let report = workspace::decode_apply_report(&report_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let report_hash = report
            .evidence_hash()
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        let reported = intent
            .generations
            .iter()
            .find_map(|e| match e.generation.evidence {
                ApplyEvidenceV1::ApplyReported {
                    report_hash,
                    journal_tip_hash,
                    result: ApplyResultV1::Success,
                    ..
                } => Some((report_hash, journal_tip_hash)),
                _ => None,
            });
        if reported != Some((report_hash, tip))
            || !matches!(
                intent.latest().phase,
                ApplyPhaseV1::ApplyReported
                    | ApplyPhaseV1::ParticipationCommitted
                    | ApplyPhaseV1::OutcomeUnknown
            )
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if intent.latest().phase != ApplyPhaseV1::ApplyReported
            && !writer
                .snapshot()?
                .state
                .source_applies
                .iter()
                .any(|record| {
                    record.candidate.run_id == self.precommit.run_id
                        && record.candidate.attempt_id == self.precommit.attempt_id
                        && record.candidate.approval_id == self.precommit.approval_id
                        && record.result.is_some()
                })
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        owner
            .observe_source(&intent.request, secrets)?
            .verify_after(&intent.request)?;
        let recovery_directory = reserved.retained_parent_directory()?;
        let generic = self
            .store
            .effect(descriptor.management_operation_id)?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        owner.require_live_capacity(
            &descriptor.capacity_plan,
            &recovery_directory,
            Some(&self.desktop_root_directory),
            ApplyOwnerV1::remaining_state_capacity(&intent, &generic.state, false)?,
            0,
            Some(writer.source_apply_result_remaining_bytes_m2a(&self.precommit, &report_bytes)?),
        )?;
        let result = writer.record_source_apply_result_evidence_m2a(
            &self.precommit,
            descriptor,
            self.precommit.reserved_result_id.clone(),
            &report_bytes,
            report_hash,
        )?;
        let marker_evidence = ApplyEvidenceV1::ParticipationCommitted {
            marker_kind: MarkerKindV1::D27Session,
            marker_hash: result.marker_evidence_hash,
            committed_revision: result.session_revision,
            sidecar_hash: Some(result.sidecar_hash),
            sidecar_bytes: Some(result.sidecar_offset),
            result_id_digest: Some(result.reserved_result_id_digest),
        };
        if intent.latest().phase == ApplyPhaseV1::ApplyReported {
            owner.append_active(marker_evidence)?;
        } else if committed.as_ref() != Some(&marker_evidence) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        #[cfg(any(test, feature = "execution-test-support"))]
        owner_stop_after(ApplyPhaseV1::ParticipationCommitted)?;
        let fresh_result = writer.record_source_apply_result_evidence_m2a(
            &self.precommit,
            descriptor,
            self.precommit.reserved_result_id.clone(),
            &report_bytes,
            report_hash,
        )?;
        let source_after = owner.observe_source(&intent.request, secrets)?;
        source_after.verify_after(&intent.request)?;
        let journal_after = reserved
            .inspect_journal(descriptor.capacity_plan.journal_bytes)
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
        if fresh_result != result
            || read_report_bytes(reserved.directory(), descriptor.capacity_plan.report_bytes)?
                != report_bytes
            || workspace::journal_tip_hash(&journal_after, descriptor.capacity_plan.journal_bytes)
                .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
                != tip
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let readback = FinalReadbackV1 {
            schema_version: 1,
            descriptor_hash: descriptor.descriptor_hash()?,
            request_hash: intent.request.request_hash()?,
            source_observation_hash: source_after.observation_hash()?,
            journal_tip_hash: Some(tip),
            report_hash: Some(report_hash),
            marker_kind: Some(MarkerKindV1::D27Session),
            marker_hash: Some(result.marker_evidence_hash),
            agreed_revision: descriptor.agreed_revision.clone(),
        };
        let completed = if let Some(started) = &self.started {
            self.store
                .complete_started(&owner.lease, started, readback.readback_hash()?)?
        } else {
            // Fresh recovery has no dispatch capability. Publish the joined
            // unknown pointer after the result marker, then certify readback.
            let unknown = self
                .store
                .unknown_effect(descriptor.management_operation_id)?
                .ok_or(ApplyOwnerError::EvidenceMismatch)?;
            let event = unknown.verified_event();
            let ManagementEffectEventV1::OutcomeUnknown { reason, .. } = event.event else {
                return Err(ApplyOwnerError::EvidenceMismatch);
            };
            owner.append_active(ApplyEvidenceV1::OutcomeUnknown {
                reason,
                journal_tip_hash: Some(tip),
                report_hash: Some(report_hash),
                marker_hash: Some(result.marker_evidence_hash),
                generic_generation: event.evidence.generation,
                generic_event_hash: event.evidence.event_hash,
                generic_sequence: event.evidence.sequence,
            })?;
            let joined_intent = owner.active()?.ok_or(ApplyOwnerError::EvidenceMismatch)?;
            let parent_fd = crate::project_mutation::open_directory_nofollow(
                reserved
                    .path()
                    .parent()
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?,
            )?;
            let parent = workspace::RecoveryParent::pin(
                &parent_fd,
                (
                    descriptor.recovery_parent_identity.device,
                    descriptor.recovery_parent_identity.inode,
                ),
                reserved
                    .path()
                    .parent()
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?,
            )
            .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
            let fresh = owner.d27_committed_readback_protected(
                &joined_intent,
                &parent,
                writer,
                &self.precommit,
                secrets,
            )?;
            if fresh != readback {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            self.store
                .complete_unknown(&owner.lease, &unknown, fresh.readback_hash()?)?
        };
        let proof = completed.verified_event().evidence;
        owner.append_active(ApplyEvidenceV1::Completed {
            readback_hash: readback.readback_hash()?,
            generic_generation: proof.generation,
            generic_event_hash: proof.event_hash,
            generic_sequence: proof.sequence,
        })?;
        #[cfg(any(test, feature = "execution-test-support"))]
        owner_stop_after(ApplyPhaseV1::Completed)?;
        owner.archive_terminal_joined(Some(&self.store))?;
        self.completed = true;
        self.owner.take();
        Ok(true)
    }
}

fn request_from_changes(
    snapshot: &crate::isolated_workspace::Snapshot,
    changes: &crate::isolated_workspace::ChangeSet,
) -> Result<crate::apply_intent::RequestV1, ApplyOwnerError> {
    use crate::apply_intent::*;
    let hash = |entry: &crate::isolated_workspace::ManifestEntry| {
        let raw = entry.sha256.ok_or(ApplyOwnerError::EvidenceMismatch)?;
        ManagementEffectHashV1::from_hex(
            &raw.iter().map(|v| format!("{v:02x}")).collect::<String>(),
        )
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)
    };
    let mut entries = Vec::with_capacity(changes.changes.len());
    for change in &changes.changes {
        entries.push(RequestEntryV1 {
            path: change
                .relative_path
                .to_str()
                .ok_or(ApplyOwnerError::EvidenceMismatch)?
                .to_owned(),
            before: change
                .before
                .as_ref()
                .map(|v| {
                    Ok::<_, ApplyOwnerError>(BeforeV1 {
                        device: v.identity.device,
                        inode: v.identity.inode,
                        hash: hash(v)?,
                        mode: v.mode,
                        observed_mode: v.observed_mode(),
                        bytes: v.bytes,
                    })
                })
                .transpose()?,
            after: change
                .after
                .as_ref()
                .map(|v| {
                    Ok::<_, ApplyOwnerError>(AfterV1 {
                        hash: hash(v)?,
                        mode: v.mode,
                        bytes: v.bytes,
                    })
                })
                .transpose()?,
        });
    }
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let request = RequestV1 {
        schema_version: 1,
        source_identity: IdentityV1 {
            device: snapshot.source_identity.device,
            inode: snapshot.source_identity.inode,
        },
        entries,
    };
    request.canonical_bytes()?;
    Ok(request)
}

type DirectoryChain = Vec<(PathBuf, (u64, u64))>;

fn directory_chain(path: &Path) -> Result<DirectoryChain, ApplyOwnerError> {
    use std::os::unix::fs::MetadataExt;
    let mut prefix = PathBuf::from("/");
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => prefix.push(name),
            _ => return Err(ApplyOwnerError::EvidenceMismatch),
        }
        let file = crate::project_mutation::open_directory_nofollow(&prefix)?;
        let metadata = file.metadata()?;
        result.push((prefix.clone(), (metadata.dev(), metadata.ino())));
    }
    Ok(result)
}

fn require_absent_child(parent: &std::fs::File, name: &str) -> Result<(), ApplyOwnerError> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name).map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Err(ApplyOwnerError::EvidenceMismatch);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOENT) {
        return Err(error.into());
    }
    Ok(())
}

fn read_report_hash(
    directory: &std::fs::File,
    maximum: u64,
) -> Result<ManagementEffectHashV1, ApplyOwnerError> {
    let bytes = read_report_bytes(directory, maximum)?;
    crate::workspace_apply::decode_apply_report(&bytes)
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?
        .evidence_hash()
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)
}

fn read_report_bytes(directory: &std::fs::File, maximum: u64) -> Result<Vec<u8>, ApplyOwnerError> {
    use std::io::Read;
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    };
    let name = std::ffi::CString::new("apply-report.v1.json").expect("fixed component");
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > maximum
    {
        return Err(ApplyOwnerError::EvidenceMismatch);
    }
    let mut bytes = Vec::new();
    (&file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let now = file.metadata()?;
    let mut attached = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            attached.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let attached = unsafe { attached.assume_init() };
    if (attached.st_dev as u64, attached.st_ino) != (metadata.dev(), metadata.ino())
        || bytes.len() as u64 != metadata.len()
        || (
            now.len(),
            now.mode(),
            now.mtime(),
            now.mtime_nsec(),
            now.ctime(),
            now.ctime_nsec(),
        ) != (
            metadata.len(),
            metadata.mode(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    {
        return Err(ApplyOwnerError::EvidenceMismatch);
    }
    crate::workspace_apply::decode_apply_report(&bytes)
        .map_err(|_| ApplyOwnerError::EvidenceMismatch)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3b_legacy_join_refuses_while_confirmed_pull_leave_preserve_source_and_peer() {
        // The auth path guard owns HOME for this process.
        if std::env::var_os("POLARIS_M3B_SYNC_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "apply_owner::tests::m3b_legacy_join_refuses_while_confirmed_pull_leave_preserve_source_and_peer",
                    "--exact",
                    "--nocapture",
                ])
                .env("POLARIS_M3B_SYNC_CHILD", "1")
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use crate::sync::{
            participation,
            repository::SyncRepository,
            share::{ShareLink, ShareManifest, SyncPaths},
        };
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let project = home.join("project");
        std::fs::create_dir(&project).unwrap();
        let peer_project = home.join("peer-project");
        std::fs::create_dir(&peer_project).unwrap();
        std::fs::write(peer_project.join("received.txt"), "peer body\n").unwrap();
        let peer = SyncRepository::open(SyncPaths::under(&home.join("peer-state"))).unwrap();
        peer.stage_from(&peer_project, &ShareManifest::default())
            .unwrap();
        let peer_head = peer.commit("fixture").unwrap();
        let owner = ApplyOwnerV1::acquire(&project).unwrap();
        let share = ShareLink {
            remote: format!(
                "file://{}",
                home.join("peer-state/sync/repository").display()
            ),
            share_id: "fixture-share".into(),
            invitation_id: "fixture-invite".into(),
            expires_unix_seconds: u64::MAX,
        };
        let before_join = fixture_bytes(&owner.state_dir);
        for accepted in [false, true] {
            assert!(
                owner
                    .join_confirmed(share.clone(), ShareManifest::default(), 1, accepted)
                    .is_err()
            );
            assert_eq!(fixture_bytes(&owner.state_dir), before_join);
        }
        assert!(
            owner
                .join_participation(share.clone(), ShareManifest::default(), 1)
                .is_err()
        );
        assert_eq!(fixture_bytes(&owner.state_dir), before_join);
        // Historical V1 participation is input data for C11/C13, not a C08
        // write through the obsolete apply adapter.
        let held = participation::Participation {
            share,
            manifest: ShareManifest::default(),
            joined_unix_seconds: 1,
            agreed_commit: None,
            applied_commit: None,
            unresolved: vec![],
        };
        let sync = owner.state_dir.join("sync");
        std::fs::create_dir(&sync).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sync, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = sync.join("participation.json");
        std::fs::write(&path, serde_json::to_vec(&held).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(owner.pull_confirmed(false).unwrap().is_none());
        let exchange = owner.pull_confirmed(true).unwrap().unwrap();
        assert!(!exchange.unresolved);
        assert!(exchange.merged_commit.is_some());
        assert!(!project.join("received.txt").exists());
        assert_eq!(peer.head(), peer_head);
        assert!(owner.leave_confirmed(false).unwrap().is_none());
        assert!(
            participation::read(&owner.state_dir)
                .unwrap()
                .participation()
                .is_some()
        );
        assert_eq!(owner.leave_confirmed(true).unwrap(), Some(true));
        assert!(matches!(
            participation::read(&owner.state_dir).unwrap(),
            participation::ParticipationReadV2::Missing
        ));
        assert_eq!(peer.head(), peer_head);
        owner.ensure_sync_idle().unwrap();
    }

    fn fixture_bytes(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        fn walk(
            root: &Path,
            directory: &Path,
            result: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                assert!(!metadata.file_type().is_symlink());
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    if metadata.is_dir() {
                        Vec::new()
                    } else {
                        std::fs::read(&path).unwrap()
                    },
                );
                if metadata.is_dir() {
                    walk(root, &path, result);
                }
            }
        }
        let mut result = std::collections::BTreeMap::new();
        walk(root, root, &mut result);
        result
    }

    #[test]
    fn m2a_c12_owner_reopens_at_durable_phase_boundaries_without_reapplying() {
        use crate::apply_intent::{ApplyPhaseV1, RevisionKindV1, RevisionV1};
        use crate::isolated_workspace::{Limits, protected_collect_changes, protected_snapshot};
        use crate::sync::participation::Participation;
        use crate::sync::share::{ShareLink, ShareManifest};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        for phase in [
            ApplyPhaseV1::Prepared,
            ApplyPhaseV1::RecoveryPrepared,
            ApplyPhaseV1::EffectStarted,
            ApplyPhaseV1::ApplyReported,
            ApplyPhaseV1::ParticipationCommitted,
            ApplyPhaseV1::Completed,
        ] {
            let target = std::env::var_os("CARGO_TARGET_DIR").expect("isolated target");
            let root = tempfile::Builder::new()
                .prefix("c12-owner-reopen-")
                .tempdir_in(target)
                .unwrap();
            let root_path = root.path().canonicalize().unwrap();
            let project = root_path.join("project");
            let state = root_path.join("state");
            let recovery = root_path.join("recovery");
            for path in [&project, &state, &recovery, &state.join("sync")] {
                std::fs::create_dir(path).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            // The fixture is its own project even beneath the repository's target.
            std::fs::create_dir(project.join(".git")).unwrap();
            assert_eq!(crate::project::resolve_root(&project), project);
            std::fs::write(project.join("main.rs"), b"before\n").unwrap();
            let revision = RevisionV1 {
                kind: RevisionKindV1::GitSha1,
                value: "b".repeat(40),
            };
            let mut participation = Participation {
                share: ShareLink {
                    remote: "file:///test-repository".into(),
                    share_id: "project-1".into(),
                    invitation_id: "invite-1".into(),
                    expires_unix_seconds: u64::MAX,
                },
                manifest: ShareManifest::default(),
                joined_unix_seconds: 1,
                agreed_commit: Some(revision.value.clone()),
                applied_commit: None,
                unresolved: vec![],
            };
            std::fs::write(
                state.join("sync/participation.json"),
                serde_json::to_vec(&participation).unwrap(),
            )
            .unwrap();
            std::fs::set_permissions(
                state.join("sync/participation.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            participation.applied_commit = participation.agreed_commit.clone();
            let limits = Limits {
                max_entries: 64,
                max_files: 32,
                max_file_bytes: 4096,
                max_total_bytes: 65536,
                max_depth: 8,
            };
            crate::rewind_workspace::WorkspaceOwner::open(
                &project,
                &state.join("rewind-workspace"),
                Default::default(),
            )
            .unwrap();
            let lease_owner =
                ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
            let owner =
                ApplyOwnerV1::with_lease(&project, &state, lease_owner.try_mutation().unwrap())
                    .unwrap();
            let parent_file = std::fs::File::open(&recovery).unwrap();
            let meta = parent_file.metadata().unwrap();
            let parent = crate::workspace_apply::RecoveryParent::pin(
                &parent_file,
                (meta.dev(), meta.ino()),
                &recovery,
            )
            .unwrap();
            let snapshot = protected_snapshot(&project, limits).unwrap();
            std::fs::write(snapshot.path().join("main.rs"), b"after\n").unwrap();
            let changes = protected_collect_changes(&snapshot, limits).unwrap();
            OWNER_STOP.with(|slot| slot.set(Some(phase)));
            assert!(
                matches!(
                    owner.apply_c12(
                        C12ApplyAdmissionV1 {
                            management_operation_id: ManagementOperationIdV1::random().unwrap(),
                            agreed_revision: revision.clone(),
                            staged_revision: revision,
                            previous_applied_revision: None,
                            target_participation: serde_json::to_vec(&participation).unwrap()
                        },
                        &snapshot,
                        &changes,
                        limits,
                        &parent
                    ),
                    Err(ApplyOwnerError::Io(_))
                ),
                "{phase:?}"
            );
            assert_eq!(owner.active().unwrap().unwrap().latest().phase, phase);
            let expected_contents = if matches!(
                phase,
                ApplyPhaseV1::Prepared
                    | ApplyPhaseV1::RecoveryPrepared
                    | ApplyPhaseV1::EffectStarted
            ) {
                b"before\n".as_slice()
            } else {
                b"after\n".as_slice()
            };
            let original_metadata = std::fs::metadata(project.join("main.rs")).unwrap();
            drop(changes);
            drop(snapshot);
            drop(owner);
            let reopened =
                ApplyOwnerV1::with_lease(&project, &state, lease_owner.try_mutation().unwrap())
                    .unwrap();
            let result = reopened.recover_current(&parent).unwrap();
            let after_metadata = std::fs::metadata(project.join("main.rs")).unwrap();
            assert_eq!(
                std::fs::read(project.join("main.rs")).unwrap(),
                expected_contents
            );
            assert_eq!(
                (
                    original_metadata.dev(),
                    original_metadata.ino(),
                    original_metadata.mtime(),
                    original_metadata.mtime_nsec(),
                    original_metadata.ctime(),
                    original_metadata.ctime_nsec()
                ),
                (
                    after_metadata.dev(),
                    after_metadata.ino(),
                    after_metadata.mtime(),
                    after_metadata.mtime_nsec(),
                    after_metadata.ctime(),
                    after_metadata.ctime_nsec()
                )
            );
            if matches!(
                phase,
                ApplyPhaseV1::EffectStarted | ApplyPhaseV1::ApplyReported
            ) {
                assert!(
                    matches!(result, ApplyOwnerRecoveryV1::OutcomeUnknown(_)),
                    "{phase:?}"
                );
                assert_eq!(
                    reopened.active().unwrap().unwrap().latest().phase,
                    ApplyPhaseV1::OutcomeUnknown
                );
                assert!(matches!(
                    reopened.recover_current(&parent).unwrap(),
                    ApplyOwnerRecoveryV1::OutcomeUnknown(_)
                ));
            } else {
                assert!(
                    matches!(result, ApplyOwnerRecoveryV1::Archived(_)),
                    "{phase:?}"
                );
                assert!(reopened.active().unwrap().is_none());
                assert!(matches!(
                    reopened.recover_current(&parent).unwrap(),
                    ApplyOwnerRecoveryV1::Idle
                ));
            }
        }
    }

    #[test]
    fn m2a_c12_apply_commits_archives_and_allows_next_operation() {
        use crate::apply_intent::{ApplyPhaseV1, RevisionKindV1, RevisionV1};
        use crate::isolated_workspace::{Limits, protected_collect_changes, protected_snapshot};
        use crate::sync::participation::{Participation, read};
        use crate::sync::share::{ShareLink, ShareManifest};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let target = std::env::var_os("CARGO_TARGET_DIR").expect("isolated test target");
        let root = tempfile::Builder::new()
            .prefix("c12-owner-")
            .tempdir_in(target)
            .unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let project = root_path.join("project");
        let state = root_path.join("state");
        let recovery = root_path.join("recovery");
        for path in [&project, &state, &recovery, &state.join("sync")] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // The fixture is its own project even beneath the repository's target.
        std::fs::create_dir(project.join(".git")).unwrap();
        assert_eq!(crate::project::resolve_root(&project), project);
        std::fs::write(project.join("main.rs"), b"before\n").unwrap();
        let revision = RevisionV1 {
            kind: RevisionKindV1::GitSha1,
            value: "a".repeat(40),
        };
        let mut participation = Participation {
            share: ShareLink {
                remote: "file:///test-repository".into(),
                share_id: "project-1".into(),
                invitation_id: "invite-1".into(),
                expires_unix_seconds: u64::MAX,
            },
            manifest: ShareManifest::default(),
            joined_unix_seconds: 1,
            agreed_commit: Some(revision.value.clone()),
            applied_commit: None,
            unresolved: vec![],
        };
        std::fs::write(
            state.join("sync/participation.json"),
            serde_json::to_vec(&participation).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(
            state.join("sync/participation.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let limits = Limits {
            max_entries: 64,
            max_files: 32,
            max_file_bytes: 4096,
            max_total_bytes: 65536,
            max_depth: 8,
        };
        crate::rewind_workspace::WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            Default::default(),
        )
        .unwrap();
        let lease_owner =
            ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let owner = ApplyOwnerV1::with_lease(&project, &state, lease_owner.try_mutation().unwrap())
            .unwrap();
        let parent_file = std::fs::File::open(&recovery).unwrap();
        let metadata = parent_file.metadata().unwrap();
        let parent = crate::workspace_apply::RecoveryParent::pin(
            &parent_file,
            (metadata.dev(), metadata.ino()),
            &recovery,
        )
        .unwrap();
        for (index, contents) in [b"first\n".as_slice(), b"second\n".as_slice()]
            .into_iter()
            .enumerate()
        {
            let snapshot = protected_snapshot(&project, limits).unwrap();
            std::fs::write(snapshot.path().join("main.rs"), contents).unwrap();
            let changes = protected_collect_changes(&snapshot, limits).unwrap();
            let previous = (index != 0).then_some(revision.clone());
            participation.applied_commit = participation.agreed_commit.clone();
            let operation = ManagementOperationIdV1::random().unwrap();
            let admission = C12ApplyAdmissionV1 {
                management_operation_id: operation,
                agreed_revision: revision.clone(),
                staged_revision: revision.clone(),
                previous_applied_revision: previous.clone(),
                target_participation: serde_json::to_vec(&participation).unwrap(),
            };
            if index == 0 {
                let before_capacity = fixture_bytes(&root_path);
                ApplyOwnerV1::test_available_limit(Some(0));
                let refused =
                    owner.apply_c12(admission.clone(), &snapshot, &changes, limits, &parent);
                ApplyOwnerV1::test_available_limit(None);
                assert!(matches!(
                    refused,
                    Err(ApplyOwnerError::Intent(
                        crate::apply_intent::ApplyIntentError::Capacity(_)
                    ))
                ));
                assert_eq!(
                    fixture_bytes(&root_path),
                    before_capacity,
                    "fresh admission shortage published evidence"
                );
                assert!(owner.active().unwrap().is_none());

                let collision = ManagementOperationIdV1::random().unwrap();
                let mut colliding_admission = admission.clone();
                colliding_admission.management_operation_id = collision;
                let project_binding =
                    ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes());
                let key =
                    crate::apply_intent::c12_operation_key(project_binding, collision).unwrap();
                let child = recovery.join(crate::apply_intent::recovery_child(key).unwrap());
                std::fs::create_dir(&child).unwrap();
                std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
                let before = fixture_bytes(&root_path);
                assert!(matches!(
                    owner.apply_c12(
                        colliding_admission.clone(),
                        &snapshot,
                        &changes,
                        limits,
                        &parent
                    ),
                    Err(ApplyOwnerError::EvidenceMismatch)
                ));
                assert_eq!(before, fixture_bytes(&root_path));
                assert!(!state.join("apply-intents-v1").exists());
                std::fs::remove_dir(&child).unwrap();
                parent_file.sync_all().unwrap();
                let store =
                    ManagementEffectStoreV1::open_at_state_dir(&project, &state, &owner.lease)
                        .unwrap();
                let prepared = match store
                    .prepare(
                        &owner.lease,
                        store.cursor().unwrap(),
                        PrepareManagementEffectV1 {
                            operation_id: collision,
                            inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
                            scope: ManagementEffectScopeV1 {
                                project: project_binding,
                                session: Some(ManagementEffectHashV1::digest(b"test session")),
                                generation: 1,
                                revision: 1,
                                permission: ManagementPermissionScopeV1::Project,
                                account_binding: None,
                            },
                            request_hash: ManagementEffectHashV1::digest(
                                b"prior unrelated request",
                            ),
                            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
                        },
                    )
                    .unwrap()
                {
                    PrepareManagementEffectOutcomeV1::Prepared(value) => value,
                    _ => panic!("missing prepared"),
                };
                store
                    .deny(
                        &owner.lease,
                        &prepared,
                        ManagementDenialReasonV1::UserDeclined,
                    )
                    .unwrap();
                for sealed in [false, true] {
                    if sealed {
                        store.test_joined_seal(&owner.lease).unwrap();
                    }
                    let before = fixture_bytes(&root_path);
                    assert!(matches!(
                        owner.apply_c12(
                            colliding_admission.clone(),
                            &snapshot,
                            &changes,
                            limits,
                            &parent
                        ),
                        Err(ApplyOwnerError::EvidenceMismatch)
                    ));
                    assert_eq!(before, fixture_bytes(&root_path), "sealed {sealed}");
                    assert!(!state.join("apply-intents-v1").exists());
                }
            }
            let report = owner
                .apply_c12(admission, &snapshot, &changes, limits, &parent)
                .unwrap();
            assert!(report.failure.is_none(), "{:?}", report.failure);
            assert_eq!(std::fs::read(project.join("main.rs")).unwrap(), contents);
            assert_eq!(
                read(&state).unwrap(),
                crate::sync::participation::ParticipationReadV2::LegacyParticipationOnly(
                    participation.clone()
                )
            );
            assert!(owner.active().unwrap().is_none());
            let key = crate::apply_intent::c12_operation_key(
                ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes()),
                operation,
            )
            .unwrap();
            let archived = owner.intents().unwrap().archived(key).unwrap().unwrap();
            assert_eq!(archived.latest().phase, ApplyPhaseV1::Completed);
            assert!(archived.seal.is_some());
            assert_eq!(archived.generations.len(), 6);
            let generic = owner.inspect_management().unwrap();
            let ManagementEffectInspectOpenV1::Present(generic) = generic else {
                panic!("missing generic store")
            };
            let effect = generic
                .effects()
                .find(|e| e.request.operation_id == operation)
                .unwrap();
            assert!(matches!(
                effect.state,
                ManagementEffectStateV1::Completed { .. }
            ));
            ApplyOwnerV1::verify_generic_join(
                &archived,
                effect,
                &generic
                    .latest_event_evidence(operation, effect)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn m2a_owner_holds_the_shared_project_lease_without_creating_intent_or_management_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            root.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let lease_owner =
            ProjectMutationOwnerV1::at_store_root(&root.path().join("rewind-workspace")).unwrap();
        let owner = ApplyOwnerV1::with_lease(
            root.path(),
            root.path(),
            lease_owner.try_mutation().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            lease_owner.try_mutation(),
            Err(ProjectMutationError::Busy)
        ));
        assert!(owner.active().unwrap().is_none());
        assert!(!root.path().join("apply-intents-v1").exists());
        assert!(!root.path().join("management-effects-v1").exists());
        drop(owner);
        lease_owner.try_mutation().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn m2a_l04_parent_validator_rejects_five_candidates() {
        use crate::linux_l04_parent::{
            L04ParentRefusal, validate_disposable_parent_candidate as validate,
        };
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        // The local fixture does not touch USB storage. Use Linux tmpfs for
        // the distinct device even when both TMPDIR and target are on root.
        let target = std::env::var_os("CARGO_TARGET_DIR").unwrap();
        let fixture = tempfile::Builder::new()
            .prefix("l04-validator-fixture-")
            .tempdir_in(target)
            .unwrap();
        let project = fixture.path().join("project");
        let empty = fixture.path().join("empty");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&empty).unwrap();
        std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = fixture.path().join("symlink");
        symlink(&empty, &link).unwrap();
        let device = std::fs::metadata(fixture.path()).unwrap().dev();
        assert!(matches!(
            validate(&link, &project, device),
            Err(L04ParentRefusal::ParentIsSymlink)
        ));
        assert!(matches!(
            validate(Path::new("/"), &project, device),
            Err(L04ParentRefusal::ParentIsFilesystemRoot)
        ));
        assert!(matches!(
            validate(&project, &project, device),
            Err(L04ParentRefusal::ParentIsProjectRoot)
        ));
        std::fs::write(project.join("sentinel"), b"unchanged").unwrap();
        assert!(matches!(
            validate(&project, &empty, device),
            Err(L04ParentRefusal::ParentNonemptyCallerOwned)
        ));
        let other_device = std::fs::metadata("/dev/shm").unwrap().dev();
        assert_ne!(device, other_device);
        assert!(matches!(
            validate(&empty, &project, other_device),
            Err(L04ParentRefusal::ParentOnDifferentDevice)
        ));
        assert_eq!(
            std::fs::read(project.join("sentinel")).unwrap(),
            b"unchanged"
        );
        let accepted = validate(&empty, &project, device).unwrap();
        assert_eq!(
            accepted.identity(),
            (device, std::fs::metadata(&empty).unwrap().ino())
        );
        drop(accepted);
        // Remove exact test-owned entries; never pass a symlink to recursive
        // target cleanup or delete an unknown entry.
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(project.join("sentinel")).unwrap();
        std::fs::remove_dir(&project).unwrap();
        std::fs::remove_dir(&empty).unwrap();
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
        std::fs::File::open(fixture.path())
            .unwrap()
            .sync_all()
            .unwrap();
        fixture.close().unwrap();
    }
}
