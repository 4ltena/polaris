//! Production C10 orchestration. Persisted DTOs never grant execution.
#[cfg(unix)]
use super::confirmation_permission_hash;
#[cfg(any(unix, test))]
use super::push_action;
use super::{SyncPushActionV1, transport::*};
#[cfg(any(unix, test))]
use crate::{
    management_effect::ManagementEffectStateV1 as State,
    sync::{
        participation::{ParticipationReadV2, ParticipationSnapshotV2},
        repository::c10::C10RepositorySnapshotV1,
    },
};
#[cfg(unix)]
use crate::{
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectStoreV1, ManagementUnknownReasonV1,
        PrepareManagementEffectOutcomeV1, StartedManagementEffectV1, UnknownManagementEffectV1,
    },
    project_mutation::ProjectMutationOwnerV1,
    sync::{
        credential_rebind::C08IdleReadV1,
        owner::C10PendingIntentV2,
        repository::c10::{C10CommitReadbackV1, C10LocalRepositoryV1},
    },
};
use crate::{
    management_effect::{
        ManagementEffectHashV1 as Hash, ManagementEffectInspectOpenV1, ManagementEffectScopeV1,
        ManagementEffectSnapshotV1, ManagementInventoryIdV1, ManagementOperationIdV1 as Operation,
        ManagementPermissionScopeV1, ManagementRewindDispositionV1, PrepareManagementEffectV1,
        PreparedManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
    sync::{
        owner::{C10IntentReservationV2, C10IntentStoreV2},
        participation::{self, ParticipationV2},
        remote_intent::*,
        repository::c10::{C10PayloadSnapshotV1, C10RepositoryErrorV1, C10RepositoryInspectionV1},
    },
};
#[cfg(unix)]
use polaris_auth::git_credentials::GitChildOutputV1;
#[cfg(any(unix, test))]
use polaris_auth::git_credentials::GitCredentialBindingV1;
use polaris_auth::git_credentials::GitCredentialOwnerV1;
#[cfg(unix)]
use std::{collections::BTreeSet, ffi::OsString, fs, os::unix::fs::MetadataExt};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum SyncPushErrorV1 {
    #[error("c10_unavailable_platform")]
    UnavailablePlatform,
    #[error("c10_protection")]
    Protection,
    #[error("c10_stale_request")]
    Stale,
    #[error("c10_blocked")]
    Blocked,
    #[error("c10_capacity")]
    Capacity,
    #[error("c10_permission_unavailable")]
    Permission,
    #[error("c10_credential_unavailable")]
    Credential,
    #[error("c10_storage")]
    Storage,
    #[error("c10_network_before_spawn")]
    NetworkBeforeSpawn,
    #[error("c10_network_after_spawn")]
    NetworkAfterSpawn,
}
type Result<T> = std::result::Result<T, SyncPushErrorV1>;
macro_rules! fixed_error {
    ($type:ty, $variant:ident) => {
        impl From<$type> for SyncPushErrorV1 {
            fn from(_: $type) -> Self {
                Self::$variant
            }
        }
    };
}
fixed_error!(std::io::Error, Protection);
#[cfg(unix)]
fixed_error!(crate::apply_intent::ApplyIntentError, Blocked);
fixed_error!(crate::project_mutation::ProjectMutationError, Blocked);
fixed_error!(
    crate::management_effect::ManagementEffectStoreError,
    Storage
);
fixed_error!(
    crate::sync::credential_rebind::CredentialRebindErrorV1,
    Blocked
);
fixed_error!(crate::sync::owner::C10IntentStoreErrorV2, Blocked);
fixed_error!(crate::sync::participation::ParticipationErrorV2, Blocked);
fixed_error!(SyncIntentValueErrorV2, Protection);
fixed_error!(SyncRemoteValueErrorV1, Protection);
fixed_error!(
    polaris_auth::git_credentials::GitCredentialErrorV1,
    Credential
);
impl From<C10RepositoryErrorV1> for SyncPushErrorV1 {
    fn from(value: C10RepositoryErrorV1) -> Self {
        match value {
            C10RepositoryErrorV1::BeforeSpawn => Self::NetworkBeforeSpawn,
            C10RepositoryErrorV1::AfterSpawn => Self::NetworkAfterSpawn,
            _ => Self::Protection,
        }
    }
}
impl From<TransportFailureV1> for SyncPushErrorV1 {
    fn from(value: TransportFailureV1) -> Self {
        match value {
            TransportFailureV1::Child(NetworkFailureV1::BeforeSpawn) => Self::NetworkBeforeSpawn,
            _ => Self::NetworkAfterSpawn,
        }
    }
}

/// Only safe display values; no locator, source paths, content or credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPushConfirmationSummaryV1 {
    pub operation_id: Operation,
    pub request_hash: SyncDigestV1,
    pub project: SyncDigestV1,
    pub share_id: SyncShareIdV1,
    pub remote_label: String,
    pub remote_ref: SyncRemoteRefV1,
    pub manifest_digest: String,
    pub payload_digest: SyncDigestV1,
    pub file_count: usize,
    pub excluded_count: usize,
    pub expected_remote: ExpectedSyncRemoteOidV1,
    pub account_registration_id: String,
    pub action: SyncPushActionV1,
}

/// The real frontend checks its current TTY/explicit --yes/TUI confirmation
/// availability here. This method supplies no final consent or persisted grant.
pub trait SyncPushHostV1: Send {
    fn require_confirmation_path(&self, summary: &SyncPushConfirmationSummaryV1) -> Result<()>;
    /// Every recovery child requires the same explicit fresh invocation.
    fn require_recovery_invocation(&self, operation: Operation) -> Result<()>;
    /// Obtain fresh explicit consent for this durable operation/request.
    fn confirm_abandon(&self, operation: Operation, request_hash: SyncDigestV1) -> Result<()>;
    /// Check the current explicit abandon invocation before each observation.
    fn require_abandon_invocation(
        &self,
        operation: Operation,
        request_hash: SyncDigestV1,
    ) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncPushOutcomeV1 {
    NoChanges,
    Completed {
        operation_id: Operation,
        commit: SyncGitObjectIdV1,
    },
    Denied {
        operation_id: Operation,
    },
    ReconciledAbandon {
        operation_id: Operation,
    },
    OutcomeUnknown {
        operation_id: Operation,
    },
    PartialComplete {
        operation_id: Operation,
    },
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum InvocationV1 {
    Confirmation,
    Recovery,
    Abandon { request_hash: SyncDigestV1 },
}
impl SyncPushOwnerV1 {
    #[cfg(unix)]
    fn require_invocation(
        &self,
        invocation: InvocationV1,
        summary: &SyncPushConfirmationSummaryV1,
    ) -> Result<()> {
        match invocation {
            InvocationV1::Confirmation => self.host.require_confirmation_path(summary),
            InvocationV1::Recovery => self.host.require_recovery_invocation(summary.operation_id),
            InvocationV1::Abandon { request_hash } => self
                .host
                .require_abandon_invocation(summary.operation_id, request_hash),
        }
    }
}

/// This authority retains the actual C10 and management inspection guards.
/// Only this owner can construct it from a durable active Completed pair.
pub(crate) struct C10CompletedParticipationRecoveryV1<'a, 'lease> {
    owner: &'a SyncPushOwnerV1,
    lease: &'lease ProjectMutationLeaseV1,
    store: &'a C10IntentStoreV2<'lease>,
    management: &'a ManagementEffectInspectOpenV1,
    history: SyncRemoteIntentHistoryV2,
    target: ParticipationV2,
}
impl C10CompletedParticipationRecoveryV1<'_, '_> {
    pub(crate) fn state_path(&self) -> &Path {
        &self.owner.state
    }
    pub(crate) fn lease(&self) -> &ProjectMutationLeaseV1 {
        self.lease
    }
    pub(crate) fn target(&self) -> &ParticipationV2 {
        &self.target
    }
    pub(crate) fn expected_prior(&self) -> SyncDigestV1 {
        self.history.current().identity().prior_participation_digest
    }
    pub(crate) fn target_digest(&self) -> SyncDigestV1 {
        match self.history.current().state() {
            SyncRemoteIntentStateV2::Completed { receipt } => receipt.participation_digest,
            _ => unreachable!("private Completed capability"),
        }
    }
    #[cfg(test)]
    pub(crate) fn test_fault(&self) -> Option<&'static str> {
        self.owner.participation_fault
    }
    #[cfg(unix)]
    pub(crate) fn verify(&self) -> std::result::Result<(), participation::ParticipationErrorV2> {
        let checked = (|| -> Result<()> {
            self.owner.verify_state()?;
            let operation = self.history.operation_id();
            let actual = self
                .store
                .lookup(operation)?
                .ok_or(SyncPushErrorV1::Protection)?;
            if actual != self.history || !self.store.active_records().any(|h| h == &actual) {
                return Err(SyncPushErrorV1::Protection);
            }
            let ManagementEffectInspectOpenV1::Present(inspected) = self.management else {
                return Err(SyncPushErrorV1::Protection);
            };
            let effect = inspected
                .effects()
                .find(|e| e.request.operation_id == operation)
                .ok_or(SyncPushErrorV1::Protection)?;
            inspected
                .latest_event_evidence(operation, effect)?
                .ok_or(SyncPushErrorV1::Protection)?;
            validate_pair_identity(&actual, effect, self.owner.project_digest())?;
            match effect.state {
                State::EffectStarted | State::OutcomeUnknown { .. } => (),
                State::Completed { evidence_hash }
                    if evidence_hash == management_hash(actual.terminal_evidence_hash()?)? =>
                {
                    ()
                }
                _ => return Err(SyncPushErrorV1::Protection),
            }
            let SyncRemoteIntentStateV2::Completed { receipt } = actual.current().state() else {
                return Err(SyncPushErrorV1::Protection);
            };
            recovery::require_part_identity(&self.target, actual.current().identity())?;
            self.owner
                .require_binding(&self.target.credential_binding)?;
            if self.target.inner().agreed_commit.as_deref()
                != Some(receipt.intended_commit.as_str())
                || SyncDigestV1::digest(&self.target.canonical_bytes()?)
                    != receipt.participation_digest
            {
                return Err(SyncPushErrorV1::Protection);
            }
            Ok(())
        })();
        checked.map_err(|_| {
            participation::ParticipationErrorV2::Corrupt("C10 completed receipt authority")
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn verify(&self) -> std::result::Result<(), participation::ParticipationErrorV2> {
        Err(participation::ParticipationErrorV2::Unavailable)
    }
}

/// Non-cloneable read-only pins. External callers cannot alter the request.
pub struct SyncPushPreviewV1 {
    summary: SyncPushConfirmationSummaryV1,
    identity: SyncIntentIdentityV2,
    prepared: SyncLocalPreparedV2,
    participation: ParticipationV2,
    payload: C10PayloadSnapshotV1,
    repository: C10RepositoryInspectionV1,
}
impl SyncPushPreviewV1 {
    pub fn summary(&self) -> &SyncPushConfirmationSummaryV1 {
        &self.summary
    }
}

pub struct SyncPushOwnerV1 {
    project: PathBuf,
    state: PathBuf,
    state_pin: File,
    credentials: GitCredentialOwnerV1,
    host: Box<dyn SyncPushHostV1>,
    #[cfg(test)]
    test_binding: Option<GitCredentialBindingV1>,
    #[cfg(test)]
    stop_at: Option<&'static str>,
    #[cfg(test)]
    participation_fault: Option<&'static str>,
}
pub struct PreparedSyncPushV1 {
    owner: SyncPushOwnerV1,
    preview: SyncPushPreviewV1,
    lease: ProjectMutationLeaseV1,
    reservation: C10IntentReservationV2,
    prepared: Option<PreparedManagementEffectV1>,
}
impl PreparedSyncPushV1 {
    pub fn summary(&self) -> &SyncPushConfirmationSummaryV1 {
        self.preview.summary()
    }
    pub fn confirm(self) -> Result<SyncPushOutcomeV1> {
        SyncPushOwnerV1::confirm(self)
    }
    pub fn deny(self) -> Result<SyncPushOutcomeV1> {
        SyncPushOwnerV1::deny(self)
    }
}
impl SyncPushOwnerV1 {
    /// Production composition stays inside C10; frontends supply only their
    /// live confirmation/recovery channel, never Git or credential authority.
    #[cfg(unix)]
    pub fn for_project(project: &Path, host: Box<dyn SyncPushHostV1>) -> Result<Self> {
        if !cfg!(target_os = "macos") {
            return Err(SyncPushErrorV1::UnavailablePlatform);
        }
        let home = std::env::var_os("HOME").ok_or(SyncPushErrorV1::Credential)?;
        let executable = std::env::current_exe().map_err(|_| SyncPushErrorV1::Protection)?;
        let askpass = executable
            .parent()
            .ok_or(SyncPushErrorV1::Protection)?
            .join("polaris-git-askpass");
        let credentials =
            GitCredentialOwnerV1::new(Path::new(&home), Path::new("/usr/bin/git"), &askpass)?;
        Self::new(project, credentials, host)
    }

    #[cfg(not(unix))]
    pub fn for_project(_project: &Path, _host: Box<dyn SyncPushHostV1>) -> Result<Self> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }

    #[cfg(unix)]
    pub fn new(
        project: &Path,
        credentials: GitCredentialOwnerV1,
        host: Box<dyn SyncPushHostV1>,
    ) -> Result<Self> {
        if !cfg!(target_os = "macos") {
            return Err(SyncPushErrorV1::UnavailablePlatform);
        }
        let project = fs::canonicalize(crate::project::resolve_root(project))?;
        let state = crate::project::storage_root_from_env()?
            .join("state")
            .join(crate::project::project_id(&project));
        if fs::canonicalize(&state)? != state {
            return Err(SyncPushErrorV1::Protection);
        }
        let state_pin = crate::project_mutation::open_directory_nofollow(&state)?;
        let result = Self {
            project,
            state,
            state_pin,
            credentials,
            host,
            #[cfg(test)]
            test_binding: None,
            #[cfg(test)]
            stop_at: None,
            #[cfg(test)]
            participation_fault: None,
        };
        result.verify_state()?;
        Ok(result)
    }

    #[cfg(not(unix))]
    pub fn new(
        _project: &Path,
        _credentials: GitCredentialOwnerV1,
        _host: Box<dyn SyncPushHostV1>,
    ) -> Result<Self> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }
    #[cfg(unix)]
    fn checkpoint(&self, name: &'static str) -> Result<()> {
        #[cfg(test)]
        if self.stop_at == Some(name) {
            return Err(SyncPushErrorV1::Storage);
        }
        let _ = name;
        Ok(())
    }
    #[cfg(unix)]
    fn verify_state(&self) -> Result<()> {
        if fs::canonicalize(&self.project)? != self.project
            || crate::project::resolve_root(&self.project) != self.project
        {
            return Err(SyncPushErrorV1::Protection);
        }
        let held = self.state_pin.metadata()?;
        let named = crate::project_mutation::open_directory_nofollow(&self.state)?.metadata()?;
        let identity = |m: &fs::Metadata| (m.dev(), m.ino(), m.uid(), m.mode());
        if identity(&held) != identity(&named)
            || !held.is_dir()
            || held.uid() != unsafe { libc::geteuid() }
            || held.mode() & 0o7777 != 0o700
        {
            return Err(SyncPushErrorV1::Protection);
        }
        Ok(())
    }
    #[cfg(unix)]
    fn snapshot(&self) -> Result<ParticipationSnapshotV2> {
        self.verify_state()?;
        #[cfg(test)]
        if self.test_binding.is_some() {
            let mut snapshot = participation::read_snapshot(&self.state)?;
            if let ParticipationReadV2::StaleCredentialBindingOnly(part) = snapshot.classification {
                self.require_binding(&part.credential_binding)?;
                snapshot.classification = ParticipationReadV2::Current(part);
            }
            return Ok(snapshot);
        }
        Ok(participation::read_with_credential_owner(
            &self.state,
            &self.credentials,
        )?)
    }
    #[cfg(unix)]
    fn require_binding(&self, binding: &GitCredentialBindingV1) -> Result<()> {
        #[cfg(test)]
        if let Some(expected) = &self.test_binding {
            if binding != expected {
                return Err(SyncPushErrorV1::Credential);
            }
            if polaris_auth::git_credentials::ExecutableIdentityV1::inspect(&binding.git.path)?
                != binding.git
            {
                return Err(SyncPushErrorV1::Credential);
            }
            return Ok(());
        }
        self.credentials.revalidate_binding(binding)?;
        Ok(())
    }
    #[cfg(unix)]
    fn existing_network(
        &self,
        repository: &C10LocalRepositoryV1,
        binding: &GitCredentialBindingV1,
        args: &[OsString],
    ) -> std::result::Result<GitChildOutputV1, NetworkFailureV1> {
        self.require_binding(binding)
            .map_err(|_| NetworkFailureV1::BeforeSpawn)?;
        #[cfg(test)]
        if self.test_binding.is_some() {
            return repository
                .run_fixture_child(args)
                .map_err(repository_network_error);
        }
        let material = self
            .credentials
            .read_bound_material(binding)
            .map_err(|_| NetworkFailureV1::BeforeSpawn)?;
        repository
            .run_network_child(&self.credentials, &material, args)
            .map_err(repository_network_error)
    }
    #[cfg(any(unix, test))]
    fn project_digest(&self) -> SyncDigestV1 {
        SyncDigestV1::digest(self.project.as_os_str().as_encoded_bytes())
    }
    #[cfg(unix)]
    fn management(&self, lease: &ProjectMutationLeaseV1) -> Result<ManagementEffectStoreV1> {
        self.verify_state()?;
        Ok(ManagementEffectStoreV1::open_at_state_dir(
            &self.project,
            &self.state,
            lease,
        )?)
    }
    #[cfg(unix)]
    fn lease(&self) -> Result<ProjectMutationLeaseV1> {
        self.verify_state()?;
        Ok(
            ProjectMutationOwnerV1::at_store_root(&self.state.join("rewind-workspace"))?
                .try_mutation()?,
        )
    }
    #[cfg(unix)]
    pub fn preview(&self) -> Result<SyncPushPreviewV1> {
        let operation = Operation::random().map_err(|_| SyncPushErrorV1::Protection)?;
        self.preview_operation(operation, InvocationV1::Confirmation)
    }

    #[cfg(not(unix))]
    pub fn preview(&self) -> Result<SyncPushPreviewV1> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }
    #[cfg(unix)]
    fn preview_operation(
        &self,
        operation: Operation,
        invocation: InvocationV1,
    ) -> Result<SyncPushPreviewV1> {
        let snapshot = self.snapshot()?;
        let ParticipationReadV2::Current(part) = snapshot.classification else {
            return Err(SyncPushErrorV1::Credential);
        };
        part.validate()?;
        // Publishing the project before applying the agreed tree would remove
        // work received from the other devices.
        if part.inner().agreed_commit != part.inner().applied_commit {
            return Err(SyncPushErrorV1::Blocked);
        }
        let digest = snapshot.exact_digest.ok_or(SyncPushErrorV1::Protection)?;
        let payload = C10PayloadSnapshotV1::capture(&self.project, &part.inner().manifest)?;
        let repository =
            C10LocalRepositoryV1::inspect(&self.state, self.credentials.git_identity())?;
        let local = match &repository {
            C10RepositoryInspectionV1::Absent(value) => value.snapshot_clean()?,
            C10RepositoryInspectionV1::Existing(value) => value.snapshot_clean()?,
        };
        let remote = part
            .inner()
            .share
            .production_remote()
            .map_err(|_| SyncPushErrorV1::Protection)?;
        let provisional = summary(
            operation,
            self.project_digest(),
            &part,
            &payload,
            &local,
            ExpectedSyncRemoteOidV1::Missing,
            zero_digest_v2(),
        )?;
        self.require_invocation(invocation, &provisional)?;
        let expected_remote = match &repository {
            C10RepositoryInspectionV1::Absent(value) => {
                self.require_binding(&part.credential_binding)?;
                #[cfg(test)]
                let output = if self.test_binding.is_some() {
                    value.observe_exact_fixture_child(&remote)?
                } else {
                    value.observe_exact_child(
                        &self.credentials,
                        &part.credential_binding,
                        &remote,
                    )?
                };
                #[cfg(not(test))]
                let output = value.observe_exact_child(
                    &self.credentials,
                    &part.credential_binding,
                    &remote,
                )?;
                decode_exact_observation(&output)?
            }
            C10RepositoryInspectionV1::Existing(value) => {
                let runner = OwnerNetwork {
                    owner: self,
                    repository: value,
                    binding: &part.credential_binding,
                    subject: &provisional,
                    payload: Some(&payload),
                    invocation,
                    participation: Some(digest),
                };
                C10CasTransportV1::new(remote.clone(), runner).observe_exact()?
            }
        };
        let prepared = SyncLocalPreparedV2 {
            payload: payload.descriptor().clone(),
            prior_head: local.head().clone(),
            head_tree: local.descriptor().clone(),
            agreed_commit: part
                .inner()
                .agreed_commit
                .as_deref()
                .map(SyncGitObjectIdV1::parse)
                .transpose()?,
            expected_remote: expected_remote.clone(),
            prior_index_digest: local.index_digest(),
            prior_staging_digest: local.staging_digest(),
        };
        let identity = SyncIntentIdentityV2 {
            operation_id: operation,
            request_hash: zero_digest_v2(),
            project: self.project_digest(),
            share_id: SyncShareIdV1::parse(&part.inner().share.share_id)?,
            remote,
            remote_ref: SyncRemoteRefV1::parse(C10_REMOTE_REF)?,
            idempotency_hash: zero_digest_v2(),
            account_binding: part.credential_binding.clone(),
            permission_hash: confirmation_permission_hash(
                operation,
                &self.project_digest(),
                &SyncShareIdV1::parse(&part.inner().share.share_id)?,
                &part
                    .inner()
                    .share
                    .production_remote()
                    .map_err(|_| SyncPushErrorV1::Protection)?,
                &part.credential_binding,
            )?,
            payload: payload.descriptor().clone(),
            prior_participation_digest: digest,
            expected_remote_oid: expected_remote.clone(),
        }
        .bind_local_request(&prepared)?;
        let summary = summary(
            operation,
            identity.project,
            &part,
            &payload,
            &local,
            expected_remote,
            identity.request_hash,
        )?;
        self.require_invocation(invocation, &summary)?;
        payload.revalidate()?;
        if self.snapshot()?.exact_digest != Some(digest) {
            return Err(SyncPushErrorV1::Stale);
        }
        Ok(SyncPushPreviewV1 {
            summary,
            identity,
            prepared,
            participation: part,
            payload,
            repository,
        })
    }
    #[cfg(unix)]
    pub fn prepare(self, preview: SyncPushPreviewV1) -> Result<PreparedSyncPushV1> {
        let lease = self.lease()?;
        let admission = self.admit(&lease, None, false)?;
        let fresh =
            self.preview_operation(preview.identity.operation_id, InvocationV1::Confirmation)?;
        require_same_preview(&preview, &fresh)?;
        drop(fresh);
        let mut intents = C10IntentStoreV2::inspect_at_state_dir(&self.state, &lease)?;
        let reservation = intents.reserve_operation(preview.identity.operation_id)?;
        preflight_intent(&preview)?;
        drop(intents);
        let prepared = if preview.summary.action == SyncPushActionV1::NoChanges {
            None
        } else {
            let management = self.management(&lease)?;
            let cursor = management.cursor()?;
            if admission.is_some_and(|expected| expected != cursor) {
                return Err(SyncPushErrorV1::Stale);
            }
            let request = request(&preview.identity, cursor.generation())?;
            let PrepareManagementEffectOutcomeV1::Prepared(prepared) =
                management.prepare(&lease, cursor, request)?
            else {
                return Err(SyncPushErrorV1::Blocked);
            };
            Some(prepared)
        };
        self.checkpoint("prepared")?;
        Ok(PreparedSyncPushV1 {
            owner: self,
            preview,
            lease,
            reservation,
            prepared,
        })
    }

    #[cfg(not(unix))]
    pub fn prepare(self, _preview: SyncPushPreviewV1) -> Result<PreparedSyncPushV1> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }
    #[cfg(unix)]
    pub fn deny(token: PreparedSyncPushV1) -> Result<SyncPushOutcomeV1> {
        token.owner.admit(
            &token.lease,
            Some(token.preview.identity.operation_id),
            false,
        )?;
        if let Some(prepared) = &token.prepared {
            let management = token.owner.management(&token.lease)?;
            management.deny(
                &token.lease,
                prepared,
                ManagementDenialReasonV1::UserDeclined,
            )?;
            if !matches!(
                management.effect(prepared.operation_id())?.map(|s| s.state),
                Some(State::Denied { .. })
            ) {
                return Err(SyncPushErrorV1::Storage);
            }
            Ok(SyncPushOutcomeV1::Denied {
                operation_id: prepared.operation_id(),
            })
        } else {
            Ok(SyncPushOutcomeV1::NoChanges)
        }
    }

    #[cfg(not(unix))]
    pub fn deny(_token: PreparedSyncPushV1) -> Result<SyncPushOutcomeV1> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }
    #[cfg(unix)]
    pub fn confirm(mut token: PreparedSyncPushV1) -> Result<SyncPushOutcomeV1> {
        let operation = token.preview.identity.operation_id;
        token.owner.admit(&token.lease, Some(operation), false)?;
        let checked = (|| {
            let fresh = token
                .owner
                .preview_operation(operation, InvocationV1::Confirmation)?;
            require_same_preview(&token.preview, &fresh)?;
            Ok(())
        })();
        if let Err(error) = checked {
            if let Some(prepared) = &token.prepared {
                token.owner.management(&token.lease)?.deny(
                    &token.lease,
                    prepared,
                    ManagementDenialReasonV1::StaleRequest,
                )?;
            }
            return Err(error);
        }
        let Some(prepared) = token.prepared.take() else {
            return Ok(SyncPushOutcomeV1::NoChanges);
        };
        let management = token.owner.management(&token.lease)?;
        let started = management.start(&token.lease, &prepared)?;
        let mut history = SyncRemoteIntentHistoryV2::local_prepared(
            token.preview.identity.clone(),
            token.preview.prepared.clone(),
        )?;
        let mut created_readback = None;
        let result = (|| {
            token.owner.checkpoint("started")?;
            token
                .owner
                .publish(&token.lease, &mut token.reservation, &history)?;
            token.owner.checkpoint("local-prepared")?;
            let repository = match token.preview.repository {
                C10RepositoryInspectionV1::Absent(value) => value.initialize_after_local_prepared(
                    &management,
                    &started,
                    &token.lease,
                    &token.preview.payload,
                )?,
                C10RepositoryInspectionV1::Existing(value) => value,
            };
            let (repository, readback) = repository.commit_after_local_prepared(
                &management,
                &started,
                &token.lease,
                &token.preview.payload,
            )?;
            let intended = readback.commit().clone();
            let before = history.before_send(SyncBeforeSendV2 {
                intended_commit: intended.clone(),
                reopened_descriptor: readback.descriptor().clone(),
                expected_remote: token.preview.identity.expected_remote_oid.clone(),
            })?;
            created_readback = Some(readback);
            token.owner.checkpoint("commit")?;
            token
                .owner
                .publish(&token.lease, &mut token.reservation, &before)?;
            history = before;
            created_readback = None;
            token.owner.checkpoint("before-send")?;
            let runner = OwnerNetwork {
                owner: &token.owner,
                repository: &repository,
                binding: &token.preview.identity.account_binding,
                subject: &token.preview.summary,
                payload: Some(&token.preview.payload),
                invocation: InvocationV1::Confirmation,
                participation: Some(token.preview.identity.prior_participation_digest),
            };
            let mut transport =
                C10CasTransportV1::new(token.preview.identity.remote.clone(), runner);
            let source = if matches!(
                token.preview.summary.action,
                SyncPushActionV1::ReconcileKnownHead | SyncPushActionV1::NoChanges
            ) {
                SyncReceiptSourceV2::ReconciledExactRef
            } else {
                SyncReceiptSourceV2::TransportAndExactRef {
                    receipt_id: transport
                        .send_once(&intended, &token.preview.identity.expected_remote_oid)?,
                }
            };
            if transport.observe_exact()?
                != (ExpectedSyncRemoteOidV1::Present {
                    oid: intended.clone(),
                })
            {
                return Err(SyncPushErrorV1::NetworkAfterSpawn);
            }
            let mut receipt = receipt_for(
                &token.preview.identity,
                &token.preview.participation,
                &intended,
            )?;
            receipt.source = source;
            let completed = history.completed(receipt)?;
            token
                .owner
                .publish(&token.lease, &mut token.reservation, &completed)?;
            history = completed;
            token.owner.checkpoint("receipt")?;
            token.owner.finish(
                &token.lease,
                &mut token.reservation,
                &history,
                &management,
                Some(&started),
                None,
                &token.preview.participation,
            )
        })();
        match result {
            Ok(value) => Ok(value),
            Err(_) => {
                if history.is_terminal() {
                    let _ = management.mark_outcome_unknown(
                        &token.lease,
                        &started,
                        ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                    );
                    return Ok(SyncPushOutcomeV1::PartialComplete {
                        operation_id: operation,
                    });
                }
                let _ = token.owner.publish_unknown(
                    &token.lease,
                    &mut token.reservation,
                    &history,
                    SyncUnknownReasonV2::TransportUncertain,
                    created_readback,
                );
                management.mark_outcome_unknown(
                    &token.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                )?;
                Ok(SyncPushOutcomeV1::OutcomeUnknown {
                    operation_id: operation,
                })
            }
        }
    }

    #[cfg(not(unix))]
    pub fn confirm(_token: PreparedSyncPushV1) -> Result<SyncPushOutcomeV1> {
        Err(SyncPushErrorV1::UnavailablePlatform)
    }
    #[cfg(unix)]
    fn publish(
        &self,
        lease: &ProjectMutationLeaseV1,
        reservation: &mut C10IntentReservationV2,
        history: &SyncRemoteIntentHistoryV2,
    ) -> Result<()> {
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)?;
        store.publish(reservation, history)?;
        Ok(())
    }
    #[cfg(unix)]
    fn finish(
        &self,
        lease: &ProjectMutationLeaseV1,
        reservation: &mut C10IntentReservationV2,
        history: &SyncRemoteIntentHistoryV2,
        management: &ManagementEffectStoreV1,
        started: Option<&StartedManagementEffectV1>,
        unknown: Option<&UnknownManagementEffectV1>,
        prior: &ParticipationV2,
    ) -> Result<SyncPushOutcomeV1> {
        let SyncRemoteIntentStateV2::Completed { receipt } = history.current().state() else {
            return Err(SyncPushErrorV1::Protection);
        };
        let next = next_participation(prior, &receipt.intended_commit);
        let digest = self.publish_completed_participation(lease, history, &next)?;
        if digest != receipt.participation_digest {
            return Err(SyncPushErrorV1::Storage);
        }
        self.checkpoint("participation")?;
        let evidence = management_hash(history.terminal_evidence_hash()?)?;
        if let Some(started) = started {
            management.complete_started(lease, started, evidence)?;
        } else if let Some(unknown) = unknown {
            management.complete_unknown(lease, unknown, evidence)?;
        } else if !matches!(management.effect(history.operation_id())?.map(|s| s.state),
            Some(State::Completed { evidence_hash }) if evidence_hash == evidence)
        {
            return Err(SyncPushErrorV1::Protection);
        }
        self.checkpoint("management")?;
        let actual = self.snapshot()?;
        let effect = management
            .effect(history.operation_id())?
            .ok_or(SyncPushErrorV1::Protection)?;
        let mut store = C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)?;
        let proof = store.terminal_readback(reservation, &effect, &actual)?;
        store.archive_terminal(reservation, proof)?;
        Ok(SyncPushOutcomeV1::Completed {
            operation_id: history.operation_id(),
            commit: receipt.intended_commit.clone(),
        })
    }
    #[cfg(unix)]
    fn publish_unknown(
        &self,
        lease: &ProjectMutationLeaseV1,
        reservation: &mut C10IntentReservationV2,
        history: &SyncRemoteIntentHistoryV2,
        reason: SyncUnknownReasonV2,
        created: Option<C10CommitReadbackV1>,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        let (previous_state, intended_commit) = match history.current().state() {
            SyncRemoteIntentStateV2::LocalPrepared { .. } => (
                SyncPreviousStateV2::LocalPrepared,
                created.as_ref().map(|v| v.commit().clone()),
            ),
            SyncRemoteIntentStateV2::BeforeSend { before_send } => (
                SyncPreviousStateV2::BeforeSend,
                Some(before_send.intended_commit.clone()),
            ),
            SyncRemoteIntentStateV2::OutcomeUnknown { .. } => return Ok(history.clone()),
            _ => return Err(SyncPushErrorV1::Protection),
        };
        let unknown = SyncOutcomeUnknownV2 {
            previous_state,
            intended_commit,
            created_commit_proof: None,
            last_observation: SyncRemoteObservationV2::Unavailable,
            reason,
        };
        let next = if let Some(readback) = created.filter(|r| r.created()) {
            history.outcome_unknown_after_reopen(
                unknown,
                C10CreatedCommitReopenProofV2::from_git_readback(readback)?,
            )?
        } else {
            history.outcome_unknown(unknown)?
        };
        self.publish(lease, reservation, &next)?;
        Ok(next)
    }
    /// Same EX; all active/archive evidence, C08 and C12 are inspected before
    /// admission. Inspection locks are dropped before normal journal methods.
    #[cfg(unix)]
    fn admit(
        &self,
        lease: &ProjectMutationLeaseV1,
        own: Option<Operation>,
        recovering: bool,
    ) -> Result<Option<crate::management_effect::ManagementEffectCursorV1>> {
        self.verify_state()?;
        crate::apply_intent::ApplyIntentStoreV1::inspect(&self.state, lease)?.require_idle()?;
        let mut c08 = C08IdleReadV1::inspect_at_state_dir(&self.project, &self.state, lease)?;
        c08.require_idle()?;
        let (effects, cursor, budget) = match c08.management() {
            ManagementEffectInspectOpenV1::Absent => {
                (Vec::new(), None, (16 * 1024, 100_000, 16 * 1024 * 1024))
            }
            ManagementEffectInspectOpenV1::Present(value) => {
                let budget = if own.is_none() {
                    Some(value.fresh_admission_limits()?)
                } else {
                    None
                };
                (
                    value.effects().cloned().collect(),
                    Some(value.cursor()),
                    budget.unwrap_or((16 * 1024, 4, 64 * 1024)),
                )
            }
        };
        if own.is_none()
            && (budget.1 < 4
                || budget.2 < budget.0.checked_mul(4).ok_or(SyncPushErrorV1::Capacity)?)
        {
            return Err(SyncPushErrorV1::Capacity);
        }
        let store = C10IntentStoreV2::inspect_at_state_dir(&self.state, lease)?;
        let snapshot = if recovering {
            self.recovery_snapshot_from_store(
                lease,
                own.ok_or(SyncPushErrorV1::Protection)?,
                &store,
                c08.management(),
            )?
        } else {
            self.snapshot()?
        };
        if !store.legacy_diagnostics()?.is_empty() {
            return Err(SyncPushErrorV1::Blocked);
        }
        match store.pending() {
            C10PendingIntentV2::None => {}
            C10PendingIntentV2::ExactNext { operation_id, .. }
                if recovering && own == Some(*operation_id) =>
            {
                let pending = store
                    .pending_history_for_operation(*operation_id)?
                    .ok_or(SyncPushErrorV1::Protection)?;
                let effect = effects
                    .iter()
                    .find(|e| e.request.operation_id == *operation_id)
                    .ok_or(SyncPushErrorV1::Protection)?;
                validate_pair_identity(&pending, effect, self.project_digest())?;
                if !matches!(
                    effect.state,
                    State::EffectStarted | State::OutcomeUnknown { .. }
                ) {
                    return Err(SyncPushErrorV1::Blocked);
                }
            }
            _ => return Err(SyncPushErrorV1::Blocked),
        }
        let mut paired = BTreeSet::new();
        for history in store.active_records() {
            let effect = effects
                .iter()
                .find(|e| e.request.operation_id == history.operation_id())
                .ok_or(SyncPushErrorV1::Protection)?;
            validate_pair_identity(history, effect, self.project_digest())?;
            match history.current().state() {
                SyncRemoteIntentStateV2::Completed { .. } => {
                    if !matches!(effect.state, State::Completed { evidence_hash }
                        if evidence_hash == management_hash(history.terminal_evidence_hash()?)?)
                    {
                        if !recovering
                            || own != Some(history.operation_id())
                            || !matches!(
                                effect.state,
                                State::EffectStarted | State::OutcomeUnknown { .. }
                            )
                        {
                            return Err(SyncPushErrorV1::Blocked);
                        }
                    } else {
                        store.active_terminal_pair_readback(
                            history.operation_id(),
                            effect,
                            &snapshot,
                        )?;
                    }
                }
                SyncRemoteIntentStateV2::ReconciledAbandon { .. } => {
                    if matches!(effect.state, State::OutcomeUnknown { .. })
                        && recovering
                        && own == Some(history.operation_id())
                    {
                        if snapshot.exact_digest
                            != Some(history.current().identity().prior_participation_digest)
                        {
                            return Err(SyncPushErrorV1::Blocked);
                        }
                    } else {
                        store.active_terminal_pair_readback(
                            history.operation_id(),
                            effect,
                            &snapshot,
                        )?;
                    }
                }
                _ => {
                    if !recovering
                        || own != Some(history.operation_id())
                        || !matches!(
                            effect.state,
                            State::EffectStarted | State::OutcomeUnknown { .. }
                        )
                    {
                        return Err(SyncPushErrorV1::Blocked);
                    }
                }
            }
            paired.insert(history.operation_id());
        }
        let mut page_cursor = None;
        loop {
            let page = store.archive_page(page_cursor.as_ref())?;
            for history in page.records {
                let effect = effects
                    .iter()
                    .find(|e| e.request.operation_id == history.operation_id())
                    .ok_or(SyncPushErrorV1::Protection)?;
                if history.current().identity().project != self.project_digest() {
                    return Err(SyncPushErrorV1::Protection);
                }
                store.archived_terminal_pair_readback(history.operation_id(), effect)?;
                if !paired.insert(history.operation_id()) {
                    return Err(SyncPushErrorV1::Protection);
                }
            }
            match page.next {
                Some(next) => page_cursor = Some(next),
                None => break,
            }
        }
        for effect in effects {
            if effect.request.inventory.encoded() == "C10" {
                if !paired.contains(&effect.request.operation_id)
                    && !matches!(effect.state, State::Denied { .. })
                {
                    if own != Some(effect.request.operation_id)
                        || !(matches!(effect.state, State::Prepared)
                            || recovering
                                && matches!(
                                    effect.state,
                                    State::EffectStarted | State::OutcomeUnknown { .. }
                                ))
                        || effect.request.scope.project.as_hex() != self.project_digest().as_hex()
                        || effect.request.scope.session.is_some()
                        || effect.request.scope.permission != ManagementPermissionScopeV1::Remote
                        || effect.request.scope.account_binding.is_none()
                        || effect.request.rewind
                            != ManagementRewindDispositionV1::IrreversibleRemote
                    {
                        return Err(SyncPushErrorV1::Blocked);
                    }
                }
            } else if !effect.state.is_terminal() {
                return Err(SyncPushErrorV1::Blocked);
            }
        }
        c08.require_idle()?;
        drop(store);
        drop(c08);
        Ok(cursor)
    }
}

fn management_hash(value: SyncDigestV1) -> Result<Hash> {
    Hash::from_hex(&value.as_hex()).map_err(|_| SyncPushErrorV1::Protection)
}
fn request(identity: &SyncIntentIdentityV2, revision: u64) -> Result<PrepareManagementEffectV1> {
    Ok(PrepareManagementEffectV1 {
        operation_id: identity.operation_id,
        inventory: ManagementInventoryIdV1::parse("C10")
            .map_err(|_| SyncPushErrorV1::Protection)?,
        scope: ManagementEffectScopeV1 {
            project: management_hash(identity.project)?,
            session: None,
            generation: identity.account_binding.generation(),
            revision,
            permission: ManagementPermissionScopeV1::Remote,
            account_binding: Some(identity.management_account_binding_hash()?),
        },
        request_hash: management_hash(identity.request_hash)?,
        rewind: ManagementRewindDispositionV1::IrreversibleRemote,
    })
}
pub(crate) fn validate_pair_identity(
    history: &SyncRemoteIntentHistoryV2,
    effect: &ManagementEffectSnapshotV1,
    project: SyncDigestV1,
) -> Result<()> {
    history.validate()?;
    let identity = history.current().identity();
    if identity.project != project
        || effect.request != request(identity, effect.request.scope.revision)?
    {
        return Err(SyncPushErrorV1::Protection);
    }
    Ok(())
}
#[cfg(unix)]
fn require_same_preview(prior: &SyncPushPreviewV1, fresh: &SyncPushPreviewV1) -> Result<()> {
    if prior.identity != fresh.identity
        || prior.prepared != fresh.prepared
        || prior.participation != fresh.participation
        || prior.summary != fresh.summary
    {
        return Err(SyncPushErrorV1::Stale);
    }
    prior.payload.revalidate()?;
    Ok(())
}
#[cfg(any(unix, test))]
fn summary(
    operation: Operation,
    project: SyncDigestV1,
    part: &ParticipationV2,
    payload: &C10PayloadSnapshotV1,
    local: &C10RepositorySnapshotV1,
    remote: ExpectedSyncRemoteOidV1,
    request_hash: SyncDigestV1,
) -> Result<SyncPushConfirmationSummaryV1> {
    let agreed = part
        .inner()
        .agreed_commit
        .as_deref()
        .map(SyncGitObjectIdV1::parse)
        .transpose()?;
    Ok(SyncPushConfirmationSummaryV1 {
        operation_id: operation,
        request_hash,
        project,
        share_id: SyncShareIdV1::parse(&part.inner().share.share_id)?,
        remote_label: part
            .inner()
            .share
            .production_remote()
            .map_err(|_| SyncPushErrorV1::Protection)?
            .redacted_label(),
        remote_ref: SyncRemoteRefV1::parse(C10_REMOTE_REF)?,
        manifest_digest: payload.manifest_digest(),
        payload_digest: payload.descriptor().digest(),
        file_count: payload.descriptor().entry_count(),
        excluded_count: payload.excluded_entries(),
        expected_remote: remote.clone(),
        account_registration_id: format!(
            "{}:{}",
            part.credential_binding.credential_item_id,
            part.credential_binding.generation()
        ),
        action: push_action(
            payload.descriptor(),
            local.descriptor(),
            local.head(),
            agreed.as_ref(),
            &remote,
        ),
    })
}
#[cfg(any(unix, test))]
fn next_participation(prior: &ParticipationV2, intended: &SyncGitObjectIdV1) -> ParticipationV2 {
    let mut next = prior.clone();
    next.participation.agreed_commit = Some(intended.as_str().to_owned());
    next.participation.applied_commit = Some(intended.as_str().to_owned());
    next
}
#[cfg(any(unix, test))]
fn receipt_for(
    identity: &SyncIntentIdentityV2,
    prior: &ParticipationV2,
    intended: &SyncGitObjectIdV1,
) -> Result<SyncCompletedReceiptV2> {
    let next = next_participation(prior, intended);
    Ok(SyncCompletedReceiptV2 {
        identity: identity.clone(),
        intended_commit: intended.clone(),
        expected_remote: identity.expected_remote_oid.clone(),
        observed_oid: intended.clone(),
        payload: identity.payload.clone(),
        participation_digest: SyncDigestV1::digest(&next.canonical_bytes()?),
        source: SyncReceiptSourceV2::ReconciledExactRef,
    })
}

#[cfg(unix)]
struct OwnerNetwork<'a> {
    owner: &'a SyncPushOwnerV1,
    repository: &'a C10LocalRepositoryV1,
    binding: &'a GitCredentialBindingV1,
    subject: &'a SyncPushConfirmationSummaryV1,
    payload: Option<&'a C10PayloadSnapshotV1>,
    invocation: InvocationV1,
    participation: Option<SyncDigestV1>,
}
#[cfg(unix)]
impl NetworkChildV1 for OwnerNetwork<'_> {
    fn run(
        &mut self,
        args: &[OsString],
    ) -> std::result::Result<GitChildOutputV1, NetworkFailureV1> {
        let checked = (|| -> Result<_> {
            self.owner.verify_state()?;
            self.owner
                .require_invocation(self.invocation, self.subject)?;
            if let Some(payload) = self.payload {
                payload.revalidate()?;
            }
            if let Some(expected) = self.participation
                && self.owner.snapshot()?.exact_digest != Some(expected)
            {
                return Err(SyncPushErrorV1::Stale);
            }
            self.repository.revalidate_controls()?;
            self.owner.require_binding(self.binding)?;
            Ok(())
        })()
        .map_err(|_| NetworkFailureV1::BeforeSpawn)?;
        let _ = checked;
        self.owner
            .existing_network(self.repository, self.binding, args)
    }
    fn require_ancestor(
        &mut self,
        expected: &SyncGitObjectIdV1,
        intended: &SyncGitObjectIdV1,
    ) -> std::result::Result<(), AncestorFailureV1> {
        self.repository
            .require_ancestor(expected, intended)
            .map_err(|e| {
                if matches!(e, C10RepositoryErrorV1::GitFailed) {
                    AncestorFailureV1::NotAncestor
                } else {
                    AncestorFailureV1::Unavailable
                }
            })
    }
}

#[cfg(any(unix, test))]
fn repository_network_error(error: C10RepositoryErrorV1) -> NetworkFailureV1 {
    if matches!(error, C10RepositoryErrorV1::AfterSpawn) {
        NetworkFailureV1::AfterSpawn
    } else {
        NetworkFailureV1::BeforeSpawn
    }
}

#[cfg(any(unix, test))]
fn preflight_intent(preview: &SyncPushPreviewV1) -> Result<()> {
    let first = SyncRemoteIntentHistoryV2::local_prepared(
        preview.identity.clone(),
        preview.prepared.clone(),
    )?;
    let intended = SyncGitObjectIdV1::parse("f".repeat(64))?;
    let before = first.before_send(SyncBeforeSendV2 {
        intended_commit: intended.clone(),
        reopened_descriptor: preview.identity.payload.clone(),
        expected_remote: preview.identity.expected_remote_oid.clone(),
    })?;
    let unknown = before.outcome_unknown(SyncOutcomeUnknownV2 {
        previous_state: SyncPreviousStateV2::BeforeSend,
        intended_commit: Some(intended.clone()),
        created_commit_proof: None,
        last_observation: SyncRemoteObservationV2::Unavailable,
        reason: SyncUnknownReasonV2::TransportUncertain,
    })?;
    let mut receipt = receipt_for(&preview.identity, &preview.participation, &intended)?;
    receipt.source = SyncReceiptSourceV2::TransportAndExactRef {
        receipt_id: SyncRemoteReceiptIdV1::parse("f".repeat(64))?,
    };
    unknown.completed(receipt)?.canonical_bytes()?;
    unknown
        .reconciled_abandon(RestoredLocalAbandonEvidenceV2 {
            identity: preview.identity.clone(),
            prior_head: preview.prepared.prior_head.clone(),
            expected_current_head: SyncPriorHeadV2::Present {
                commit: intended.clone(),
            },
            created_commit: Some(intended),
            restored_head: preview.prepared.prior_head.clone(),
            prior_descriptor: preview.prepared.head_tree.clone(),
            reopened_descriptor: preview.prepared.head_tree.clone(),
            prior_index_digest: preview.prepared.prior_index_digest,
            restored_index_digest: preview.prepared.prior_index_digest,
            prior_staging_digest: preview.prepared.prior_staging_digest,
            restored_staging_digest: preview.prepared.prior_staging_digest,
            durable_restore_digest: preview.identity.request_hash,
            last_observation: SyncRemoteObservationV2::Unavailable,
            participation_digest: preview.identity.prior_participation_digest,
            confirmation: SyncAbandonConfirmationV2::ExplicitUserConfirmation,
        })?
        .canonical_bytes()?;
    Ok(())
}

#[cfg(test)]
mod data_tests;
mod recovery;
#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;
