//! C08 owns credential-binding replacement under one project EX lease.
//! Durable intent publication precedes ParticipationV2 CAS; no Git child runs.
#[cfg(test)]
mod data_tests;
mod store_v1;
#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;

use super::participation::{self, ParticipationReadV2, ParticipationSnapshotV2, ParticipationV2};
use super::remote_intent::SyncDigestV1;
use super::share::CanonicalHttpsRemoteV1;
use crate::management_effect::{
    ManagementDenialReasonV1, ManagementEffectHashV1 as Hash, ManagementEffectInspectOpenV1,
    ManagementEffectScopeV1, ManagementEffectSnapshotV1, ManagementEffectStateV1 as State,
    ManagementEffectStoreV1, ManagementInventoryIdV1, ManagementOperationIdV1 as Operation,
    ManagementPermissionScopeV1, ManagementRewindDispositionV1, PrepareManagementEffectOutcomeV1,
    PrepareManagementEffectV1, PreparedManagementEffectV1,
};
#[cfg(unix)]
use crate::management_effect::{ManagementEffectTailV1, ManagementUnknownReasonV1};
#[cfg(unix)]
use crate::project_mutation::ProjectMutationOwnerV1;
use crate::project_mutation::{ProjectMutationError, ProjectMutationLeaseV1};
use polaris_auth::git_credentials::{
    GitCredentialBindingV1, GitCredentialErrorV1, GitCredentialOwnerV1,
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
use std::{
    io,
    path::{Path, PathBuf},
};
use store_v1::C08IntentStoreV1;

const MAX_RECORD: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CredentialRebindErrorV1 {
    #[error(transparent)]
    Lease(#[from] ProjectMutationError),
    #[error(transparent)]
    Management(#[from] crate::management_effect::ManagementEffectStoreError),
    #[error(transparent)]
    Participation(#[from] participation::ParticipationErrorV2),
    #[error(transparent)]
    Credential(#[from] GitCredentialErrorV1),
    // OS error strings may contain caller paths; display only a fixed category.
    #[error("c08_storage")]
    Io(#[from] io::Error),
    #[error("c08_unavailable: {0}")]
    Unavailable(&'static str),
    #[error("c08_blocked: {0}")]
    Blocked(&'static str),
    #[error("c08_corrupt")]
    Corrupt,
    #[error("c08_stale")]
    Stale,
}
type Result<T> = std::result::Result<T, CredentialRebindErrorV1>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum C08OperationKindV1 {
    Join,
    Rebind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct C08ConfirmationSummaryV1 {
    pub operation: C08OperationKindV1,
    pub project: Hash,
    pub share_id: String,
    pub remote: CanonicalHttpsRemoteV1,
    pub previous_generation: Option<u64>,
    pub next_generation: u64,
    pub joined_unix_seconds: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum C08IntentStateV1 {
    BeforeWrite,
    OutcomeUnknown,
    Completed { participation_digest: SyncDigestV1 },
    ReconciledAbandon { evidence_hash: Hash },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct C08RebindIntentV1 {
    pub schema_version: u8,
    pub operation_id: Operation,
    pub request_hash: Hash,
    pub project: Hash,
    pub share_id: String,
    pub old_digest: Option<SyncDigestV1>,
    pub next: ParticipationV2,
    pub prospective_binding: GitCredentialBindingV1,
    pub confirmation: C08ConfirmationSummaryV1,
    pub sequence: u64,
    pub previous_hash: Hash,
    pub state: C08IntentStateV1,
    pub record_hash: Hash,
}
#[derive(Serialize)]
struct IntentHashInput<'a> {
    schema_version: u8,
    operation_id: Operation,
    request_hash: Hash,
    project: Hash,
    share_id: &'a str,
    old_digest: Option<SyncDigestV1>,
    next: &'a ParticipationV2,
    prospective_binding: &'a GitCredentialBindingV1,
    confirmation: &'a C08ConfirmationSummaryV1,
    sequence: u64,
    previous_hash: Hash,
    state: &'a C08IntentStateV1,
}
impl C08RebindIntentV1 {
    fn computed_hash(&self) -> Result<Hash> {
        encoded_hash(
            b"polaris-c08-intent-record-v1\0",
            &IntentHashInput {
                schema_version: self.schema_version,
                operation_id: self.operation_id,
                request_hash: self.request_hash,
                project: self.project,
                share_id: &self.share_id,
                old_digest: self.old_digest,
                next: &self.next,
                prospective_binding: &self.prospective_binding,
                confirmation: &self.confirmation,
                sequence: self.sequence,
                previous_hash: self.previous_hash,
                state: &self.state,
            },
        )
    }
    fn validate(&self) -> Result<()> {
        self.next.validate()?;
        if self.schema_version != 1
            || self.sequence == 0
            || self.share_id != self.next.inner().share.share_id
            || self.project != self.confirmation.project
            || self.share_id != self.confirmation.share_id
            || self.prospective_binding != self.next.credential_binding
            || self.confirmation.remote
                != self
                    .next
                    .inner()
                    .share
                    .production_remote()
                    .map_err(|_| CredentialRebindErrorV1::Corrupt)?
            || self.confirmation.next_generation != self.prospective_binding.generation()
            || self
                .confirmation
                .previous_generation
                .map_or(Some(1), |v| v.checked_add(1))
                != Some(self.confirmation.next_generation)
            || self.request_hash
                != request_hash(
                    self.project,
                    self.old_digest,
                    &self.next,
                    &self.confirmation,
                )?
            || self.record_hash != self.computed_hash()?
            || matches!(&self.state, C08IntentStateV1::Completed { participation_digest }
                if *participation_digest != SyncDigestV1::digest(&self.next.canonical_bytes()?))
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        if let C08IntentStateV1::ReconciledAbandon { evidence_hash } = self.state
            && evidence_hash == zero_hash()?
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        Ok(())
    }
    fn next_record(&self, state: C08IntentStateV1) -> Result<Self> {
        let mut next = self.clone();
        next.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        next.previous_hash = self.record_hash;
        next.state = state;
        next.record_hash = next.computed_hash()?;
        next.validate()?;
        Ok(next)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct C08NoIntentAbandonEvidenceV1 {
    pub schema_version: u8,
    pub operation_id: Operation,
    pub management_request: PrepareManagementEffectV1,
    pub old_digest: Option<SyncDigestV1>,
    pub current_digest: Option<SyncDigestV1>,
    pub next: ParticipationV2,
    pub prospective_binding: GitCredentialBindingV1,
    pub confirmation: C08ConfirmationSummaryV1,
    pub c08_inventory_digest: Hash,
    pub user_confirmed: bool,
    pub evidence_hash: Hash,
}
impl C08NoIntentAbandonEvidenceV1 {
    fn computed_hash(&self) -> Result<Hash> {
        encoded_hash(
            b"polaris-c08-no-intent-abandon-v1\0",
            &(
                self.schema_version,
                self.operation_id,
                &self.management_request,
                self.old_digest,
                self.current_digest,
                &self.next,
                &self.prospective_binding,
                &self.confirmation,
                self.c08_inventory_digest,
                self.user_confirmed,
            ),
        )
    }
    fn validate(&self) -> Result<()> {
        self.next.validate()?;
        if self.schema_version != 1
            || self.operation_id != self.management_request.operation_id
            || self.old_digest != self.current_digest
            || !self.user_confirmed
            || self.prospective_binding != self.next.credential_binding
            || self.management_request.inventory.encoded() != "C08"
            || self.management_request.scope.project != self.confirmation.project
            || self.management_request.scope.session.is_some()
            || self.management_request.scope.account_binding.is_some()
            || self.management_request.scope.permission != ManagementPermissionScopeV1::Project
            || self.management_request.rewind != ManagementRewindDispositionV1::IrreversibleLocal
            || self.management_request.scope.generation != self.prospective_binding.generation()
            || self.management_request.request_hash
                != request_hash(
                    self.confirmation.project,
                    self.old_digest,
                    &self.next,
                    &self.confirmation,
                )?
            || self.evidence_hash != self.computed_hash()?
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum C08StoredRecordV1 {
    IntentHistory {
        records: Vec<C08RebindIntentV1>,
    },
    NoIntentAbandon {
        evidence: Box<C08NoIntentAbandonEvidenceV1>,
    },
}
impl C08StoredRecordV1 {
    fn operation_id(&self) -> Operation {
        match self {
            Self::IntentHistory { records } => records[0].operation_id,
            Self::NoIntentAbandon { evidence } => evidence.operation_id,
        }
    }
    fn latest(&self) -> Option<&C08RebindIntentV1> {
        match self {
            Self::IntentHistory { records } => records.last(),
            _ => None,
        }
    }
    fn is_terminal(&self) -> bool {
        match self {
            Self::IntentHistory { records } => records.last().is_some_and(|r| {
                matches!(
                    r.state,
                    C08IntentStateV1::Completed { .. } | C08IntentStateV1::ReconciledAbandon { .. }
                )
            }),
            Self::NoIntentAbandon { .. } => true,
        }
    }
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        match self {
            Self::IntentHistory { records } => {
                if records.is_empty() || records.len() > 3 {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
                for (index, record) in records.iter().enumerate() {
                    record.validate()?;
                    if index == 0 {
                        if record.sequence != 1
                            || record.previous_hash != zero_hash()?
                            || record.state != C08IntentStateV1::BeforeWrite
                        {
                            return Err(CredentialRebindErrorV1::Corrupt);
                        }
                    } else {
                        let previous = &records[index - 1];
                        let state = &record.state;
                        let mut expected = previous.next_record(state.clone())?;
                        expected.state = state.clone();
                        if &expected != record
                            || !matches!(
                                (&previous.state, state),
                                (
                                    C08IntentStateV1::BeforeWrite,
                                    C08IntentStateV1::OutcomeUnknown
                                        | C08IntentStateV1::Completed { .. }
                                ) | (
                                    C08IntentStateV1::OutcomeUnknown,
                                    C08IntentStateV1::Completed { .. }
                                        | C08IntentStateV1::ReconciledAbandon { .. }
                                )
                            )
                        {
                            return Err(CredentialRebindErrorV1::Corrupt);
                        }
                    }
                }
            }
            Self::NoIntentAbandon { evidence } => evidence.validate()?,
        }
        let bytes = serde_json::to_vec(self).map_err(|_| CredentialRebindErrorV1::Corrupt)?;
        if bytes.len() > MAX_RECORD {
            return Err(CredentialRebindErrorV1::Blocked("record capacity"));
        }
        Ok(bytes)
    }
    fn is_extension_of(&self, prior: &Self) -> Result<()> {
        self.canonical_bytes()?;
        prior.canonical_bytes()?;
        if self == prior {
            return Ok(());
        }
        if let (Self::IntentHistory { records: before }, Self::IntentHistory { records: after }) =
            (prior, self)
            && !prior.is_terminal()
            && after.len() == before.len() + 1
            && after.starts_with(before)
        {
            return Ok(());
        }
        Err(CredentialRebindErrorV1::Corrupt)
    }
    fn evidence_hash(&self) -> Result<Hash> {
        encoded_hash(b"polaris-c08-stored-evidence-v1\0", self)
    }
    fn append(&self, state: C08IntentStateV1) -> Result<Self> {
        let Self::IntentHistory { records } = self else {
            return Err(CredentialRebindErrorV1::Corrupt);
        };
        let mut records = records.clone();
        records.push(
            records
                .last()
                .ok_or(CredentialRebindErrorV1::Corrupt)?
                .next_record(state)?,
        );
        let next = Self::IntentHistory { records };
        next.canonical_bytes()?;
        Ok(next)
    }
}

/// Read-only C08 pair inspection borrowed under the caller's actual EX.
/// It deliberately does not apply C08's remote-intent/admission policy to C10.
pub(crate) struct C08IdleReadV1<'a> {
    lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    state_dir: PathBuf,
    project: Hash,
    management: ManagementEffectInspectOpenV1,
    intents: C08IntentStoreV1,
}

impl<'a> C08IdleReadV1<'a> {
    pub(crate) fn inspect_at_state_dir(
        project: &Path,
        state: &Path,
        lease: &'a ProjectMutationLeaseV1,
    ) -> Result<Self> {
        Self::inspect_with_read_lease(
            project,
            state,
            crate::project_mutation::ProjectReadLeaseV1::Mutation(lease),
        )
    }
    #[cfg(unix)]
    pub(crate) fn inspect_with_read_lease(
        project: &Path,
        state: &Path,
        lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    ) -> Result<Self> {
        lease
            .verify(state)
            .map_err(|_| CredentialRebindErrorV1::Corrupt)?;
        if !project.is_absolute()
            || fs::canonicalize(project)? != project
            || !fs::symlink_metadata(project)?.is_dir()
            || !state.is_absolute()
            || fs::canonicalize(state)? != state
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        // Keep lock order project -> management -> C08. Do not call normal
        // effect() while this inspection retains its management owner lock.
        let management =
            ManagementEffectStoreV1::inspect_open_with_read_lease(project, state, lease)?;
        let intents = C08IntentStoreV1::inspect(state)?;
        let mut result = Self {
            lease,
            state_dir: state.to_owned(),
            project: Hash::digest(project.as_os_str().as_encoded_bytes()),
            management,
            intents,
        };
        result.require_idle()?;
        Ok(result)
    }

    pub(crate) fn management(&self) -> &ManagementEffectInspectOpenV1 {
        &self.management
    }

    #[cfg(unix)]
    pub(crate) fn require_idle(&mut self) -> Result<Hash> {
        self.lease
            .verify(&self.state_dir)
            .map_err(|_| CredentialRebindErrorV1::Corrupt)?;
        let expected = self.state_dir.join("rewind-workspace");
        let rewind = crate::project_mutation::open_directory_nofollow(&expected)?;
        let metadata = rewind.metadata()?;
        if self.lease.store_root() != expected
            || self.lease.store_identity() != (metadata.dev(), metadata.ino())
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        self.intents.refresh()?;
        let effects: Vec<_> = match &self.management {
            ManagementEffectInspectOpenV1::Absent => {
                let directory = crate::project_mutation::open_directory_nofollow(&self.state_dir)?;
                let before = directory.metadata()?;
                for name in [c"management-effects-v1", c"management-effects-v2"] {
                    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                    if unsafe {
                        libc::fstatat(
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            stat.as_mut_ptr(),
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    } == 0
                    {
                        return Err(CredentialRebindErrorV1::Blocked(
                            "management appeared during read",
                        ));
                    }
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ENOENT) {
                        return Err(error.into());
                    }
                }
                let full = |m: &fs::Metadata| {
                    (
                        m.dev(),
                        m.ino(),
                        m.uid(),
                        m.mode(),
                        m.nlink(),
                        m.len(),
                        m.mtime(),
                        m.mtime_nsec(),
                        m.ctime(),
                        m.ctime_nsec(),
                    )
                };
                let named = crate::project_mutation::open_directory_nofollow(&self.state_dir)?;
                if full(&before) != full(&directory.metadata()?)
                    || full(&before) != full(&named.metadata()?)
                {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
                self.intents.refresh()?;
                Vec::new()
            }
            ManagementEffectInspectOpenV1::Present(inspected) => {
                if inspected.tail() != ManagementEffectTailV1::Complete {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
                // The verified event lookup rechecks every held/named journal
                // pin without acquiring the already held owner lock again.
                if let Some(effect) = inspected.effects().next() {
                    inspected
                        .latest_event_evidence(effect.request.operation_id, effect)?
                        .ok_or(CredentialRebindErrorV1::Corrupt)?;
                } else {
                    inspected.verify_read_snapshot()?;
                }
                inspected.effects().cloned().collect()
            }
        };
        for record in self.intents.records() {
            let effect = effects
                .iter()
                .find(|effect| effect.request.operation_id == record.operation_id())
                .ok_or(CredentialRebindErrorV1::Corrupt)?;
            validate_pair(effect, Some(record), self.project)?;
            if !record.is_terminal() || !effect.state.is_terminal() {
                return Err(CredentialRebindErrorV1::Blocked("C08 is not idle"));
            }
        }
        for effect in effects
            .iter()
            .filter(|effect| effect.request.inventory.encoded() == "C08")
        {
            validate_pair(
                effect,
                self.intents.lookup(effect.request.operation_id),
                self.project,
            )?;
            if !effect.state.is_terminal() {
                return Err(CredentialRebindErrorV1::Blocked("C08 is not idle"));
            }
        }
        Ok(self.intents.inventory_digest())
    }
}

#[derive(Clone, Debug)]
pub struct CredentialRebindPreviewV1 {
    pub operation_id: Operation,
    pub old_digest: Option<SyncDigestV1>,
    pub next: ParticipationV2,
    pub prospective_binding: GitCredentialBindingV1,
    pub confirmation: C08ConfirmationSummaryV1,
    request_hash: Hash,
}
enum BindingVerifier {
    Production(GitCredentialOwnerV1),
    #[cfg(test)]
    Test(bool),
}
pub struct CredentialRebindOwnerV1 {
    project_root: PathBuf,
    state_dir: PathBuf,
    lease: ProjectMutationLeaseV1,
    credentials: BindingVerifier,
    intents: C08IntentStoreV1,
    #[cfg(test)]
    stop_at: Option<&'static str>,
}
pub struct PreparedRebindTokenV1 {
    owner: Option<CredentialRebindOwnerV1>,
    preview: CredentialRebindPreviewV1,
    prepared: PreparedManagementEffectV1,
}
impl CredentialRebindOwnerV1 {
    pub fn acquire(project_root: &Path, credentials: GitCredentialOwnerV1) -> Result<Self> {
        Self::acquire_inner(project_root, BindingVerifier::Production(credentials))
    }
    #[cfg(unix)]
    fn acquire_inner(project_root: &Path, credentials: BindingVerifier) -> Result<Self> {
        let project_root = fs::canonicalize(project_root)?;
        let state_dir = fs::canonicalize(crate::project::state_dir(&project_root)?)?;
        let lease = ProjectMutationOwnerV1::for_project(&project_root)?.try_mutation()?;
        let intents = C08IntentStoreV1::inspect(&state_dir)?;
        Ok(Self {
            project_root,
            state_dir,
            lease,
            credentials,
            intents,
            #[cfg(test)]
            stop_at: None,
        })
    }
    fn project_hash(&self) -> Hash {
        Hash::digest(self.project_root.as_os_str().as_encoded_bytes())
    }
    fn admission_snapshot(&self) -> Result<ParticipationSnapshotV2> {
        match &self.credentials {
            BindingVerifier::Production(owner) => Ok(participation::read_with_credential_owner(
                &self.state_dir,
                owner,
            )?),
            #[cfg(test)]
            BindingVerifier::Test(_) => Ok(participation::read_snapshot(&self.state_dir)?),
        }
    }
    fn verify_binding(&self, binding: &GitCredentialBindingV1) -> Result<()> {
        match &self.credentials {
            BindingVerifier::Production(owner) => owner
                .revalidate_binding(binding)
                .map(|_| ())
                .map_err(Into::into),
            #[cfg(test)]
            BindingVerifier::Test(valid) => {
                if *valid {
                    Ok(())
                } else {
                    Err(CredentialRebindErrorV1::Stale)
                }
            }
        }
    }
    fn checkpoint(&self, name: &'static str) -> Result<()> {
        #[cfg(test)]
        if self.stop_at == Some(name) {
            return Err(CredentialRebindErrorV1::Blocked("test interruption"));
        }
        let _ = name;
        Ok(())
    }
    /// The frontend selects the prospective binding first. Preview neither
    /// asks for credentials nor creates the fingerprint key or intent store.
    pub fn select_candidate(
        &self,
        remote: &CanonicalHttpsRemoteV1,
        account: &str,
        generation: u64,
        choice: Option<&str>,
    ) -> Result<GitCredentialBindingV1> {
        match &self.credentials {
            BindingVerifier::Production(owner) => {
                let (binding, material) = owner.select(remote, account, generation, choice)?;
                drop(material);
                Ok(binding)
            }
            #[cfg(test)]
            BindingVerifier::Test(_) => Err(CredentialRebindErrorV1::Unavailable("test selection")),
        }
    }
    pub fn recovery_confirmation(&self, id: Operation) -> Result<Option<C08ConfirmationSummaryV1>> {
        let inspected = self.inspect_management(Some(id))?;
        drop(inspected);
        Ok(self.intents.lookup(id).map(|record| match record {
            C08StoredRecordV1::IntentHistory { records } => records[0].confirmation.clone(),
            C08StoredRecordV1::NoIntentAbandon { evidence } => evidence.confirmation.clone(),
        }))
    }
    pub fn preview(&self, binding: GitCredentialBindingV1) -> Result<CredentialRebindPreviewV1> {
        binding.validate()?;
        let snapshot = self.admission_snapshot()?;
        proposal(
            self.project_hash(),
            &snapshot,
            binding,
            Operation::random()
                .map_err(|_| CredentialRebindErrorV1::Unavailable("operation id"))?,
        )
    }
    pub fn preview_join(
        &self,
        binding: GitCredentialBindingV1,
        share: super::share::ShareLink,
        manifest: super::share::ShareManifest,
        now: u64,
    ) -> Result<CredentialRebindPreviewV1> {
        let current = participation::read_snapshot(&self.state_dir)?;
        join_proposal(
            self.project_hash(),
            &current,
            binding,
            share,
            manifest,
            now,
            Operation::random()
                .map_err(|_| CredentialRebindErrorV1::Unavailable("operation id"))?,
        )
    }
    fn refresh_proposal(
        &self,
        current: &ParticipationSnapshotV2,
        proposed: &CredentialRebindPreviewV1,
    ) -> Result<CredentialRebindPreviewV1> {
        match proposed.confirmation.operation {
            C08OperationKindV1::Rebind => proposal(
                self.project_hash(),
                current,
                proposed.prospective_binding.clone(),
                proposed.operation_id,
            ),
            C08OperationKindV1::Join => join_proposal(
                self.project_hash(),
                current,
                proposed.prospective_binding.clone(),
                proposed.next.inner().share.clone(),
                proposed.next.inner().manifest.clone(),
                proposed.next.inner().joined_unix_seconds,
                proposed.operation_id,
            ),
        }
    }
    pub fn prepare(mut self, preview: CredentialRebindPreviewV1) -> Result<PreparedRebindTokenV1> {
        let fresh = self.admission_snapshot()?;
        validate_fresh_invitation(&preview)?;
        let expected = self.refresh_proposal(&fresh, &preview)?;
        if !same_preview(&expected, &preview) {
            return Err(CredentialRebindErrorV1::Stale);
        }
        self.intents.refresh()?;
        self.intents.reserve_operation(preview.operation_id)?;
        let inspected = self.inspect_management(None)?;
        let budget = match &inspected {
            ManagementEffectInspectOpenV1::Present(value) => value.fresh_admission_limits()?,
            ManagementEffectInspectOpenV1::Absent => (16 * 1024, 100_000, 16 * 1024 * 1024),
        };
        check_management_capacity(budget)?;
        // Bound the maximum history, not just the first record, before Prepared.
        let first = self.first_intent(&preview)?;
        let complete = first.append(C08IntentStateV1::OutcomeUnknown)?.append(
            C08IntentStateV1::Completed {
                participation_digest: target_digest(&preview.next)?,
            },
        )?;
        complete.canonical_bytes()?;
        drop(inspected);
        let effects = self.open_management()?;
        let request = PrepareManagementEffectV1 {
            operation_id: preview.operation_id,
            inventory: ManagementInventoryIdV1::parse("C08")
                .map_err(|_| CredentialRebindErrorV1::Corrupt)?,
            scope: ManagementEffectScopeV1 {
                project: self.project_hash(),
                session: None,
                generation: preview.prospective_binding.generation(),
                revision: effects.cursor()?.generation(),
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: preview.request_hash,
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) =
            effects.prepare(&self.lease, effects.cursor()?, request)?
        else {
            return Err(CredentialRebindErrorV1::Blocked("operation exists"));
        };
        self.checkpoint("prepared")?;
        Ok(PreparedRebindTokenV1 {
            owner: Some(self),
            preview,
            prepared,
        })
    }
    fn first_intent(&self, preview: &CredentialRebindPreviewV1) -> Result<C08StoredRecordV1> {
        let mut first = C08RebindIntentV1 {
            schema_version: 1,
            operation_id: preview.operation_id,
            request_hash: preview.request_hash,
            project: self.project_hash(),
            share_id: preview.next.inner().share.share_id.clone(),
            old_digest: preview.old_digest,
            next: preview.next.clone(),
            prospective_binding: preview.prospective_binding.clone(),
            confirmation: preview.confirmation.clone(),
            sequence: 1,
            previous_hash: zero_hash()?,
            state: C08IntentStateV1::BeforeWrite,
            record_hash: zero_hash()?,
        };
        first.record_hash = first.computed_hash()?;
        let record = C08StoredRecordV1::IntentHistory {
            records: vec![first],
        };
        record.canonical_bytes()?;
        Ok(record)
    }
    fn open_management(&self) -> Result<ManagementEffectStoreV1> {
        Ok(ManagementEffectStoreV1::open_at_state_dir(
            &self.project_root,
            &self.state_dir,
            &self.lease,
        )?)
    }
    #[cfg(unix)]
    fn inspect_management(&self, own: Option<Operation>) -> Result<ManagementEffectInspectOpenV1> {
        reject_legacy_remote_intents(&self.state_dir)?;
        crate::apply_intent::ApplyIntentStoreV1::inspect(&self.state_dir, &self.lease)
            .and_then(|store| store.require_idle())
            .map_err(|_| CredentialRebindErrorV1::Blocked("apply intent is not idle"))?;
        let inspected = ManagementEffectStoreV1::inspect_open_at_state_dir(
            &self.project_root,
            &self.state_dir,
            &self.lease,
        )?;
        let effects: Vec<_> = match &inspected {
            ManagementEffectInspectOpenV1::Absent => Vec::new(),
            ManagementEffectInspectOpenV1::Present(value) => {
                if value.tail() != ManagementEffectTailV1::Complete {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
                value.effects().cloned().collect()
            }
        };
        for record in self.intents.records() {
            record.canonical_bytes()?;
            let effect = effects
                .iter()
                .find(|e| e.request.operation_id == record.operation_id())
                .ok_or(CredentialRebindErrorV1::Corrupt)?;
            validate_pair(effect, Some(record), self.project_hash())?;
            if !record.is_terminal() && own != Some(record.operation_id()) {
                return Err(CredentialRebindErrorV1::Blocked("other intent"));
            }
        }
        for effect in &effects {
            if effect.request.inventory.encoded() == "C08" {
                validate_pair(
                    effect,
                    self.intents.lookup(effect.request.operation_id),
                    self.project_hash(),
                )?;
            }
            if effect.state.blocks_new_work() && own != Some(effect.request.operation_id) {
                return Err(CredentialRebindErrorV1::Blocked(
                    "other management operation",
                ));
            }
        }
        if let Some(id) = own
            && effects
                .iter()
                .find(|e| e.request.operation_id == id)
                .is_none_or(|e| e.request.inventory.encoded() != "C08")
        {
            return Err(CredentialRebindErrorV1::Blocked(
                "operation is not recoverable",
            ));
        }
        if super::composite::inspect_c10_pairs(
            &self.project_root,
            &self.state_dir,
            crate::project_mutation::ProjectReadLeaseV1::Mutation(&self.lease),
            &inspected,
        )
        .map_err(|_| CredentialRebindErrorV1::Blocked("C10 pair is not idle"))?
        .is_some()
        {
            return Err(CredentialRebindErrorV1::Blocked("C10 is not idle"));
        }
        Ok(inspected)
    }
    /// Existing-intent recovery does not need re-selection. A no-intent
    /// abandon instead takes a newly reconstructed exact prospective binding.
    pub fn recover(self, operation_id: Operation) -> Result<()> {
        self.recover_or_abandon(operation_id, None, false)
    }
    pub fn abandon(
        self,
        operation_id: Operation,
        prospective_binding: Option<CredentialRebindPreviewV1>,
    ) -> Result<()> {
        self.recover_or_abandon(operation_id, prospective_binding, true)
    }
    pub fn recovery_needs_selection(&self, operation_id: Operation) -> Result<bool> {
        let inspected = self.inspect_management(Some(operation_id))?;
        Ok(self.intents.lookup(operation_id).is_none()
            && match &inspected {
                ManagementEffectInspectOpenV1::Present(value) => value
                    .effects()
                    .find(|e| e.request.operation_id == operation_id)
                    .is_some_and(|e| !e.state.is_terminal()),
                _ => false,
            })
    }
    #[cfg(unix)]
    fn recover_or_abandon(
        mut self,
        id: Operation,
        prospective: Option<CredentialRebindPreviewV1>,
        abandon: bool,
    ) -> Result<()> {
        self.intents.refresh()?;
        let inspected = self.inspect_management(Some(id))?;
        let effect = match &inspected {
            ManagementEffectInspectOpenV1::Present(value) => value
                .effects()
                .find(|e| e.request.operation_id == id)
                .cloned(),
            _ => None,
        }
        .ok_or(CredentialRebindErrorV1::Corrupt)?;
        let record = self.intents.lookup(id).cloned();
        if effect.state.is_terminal() {
            let allowed = if abandon {
                matches!(
                    effect.state,
                    State::Denied { .. } | State::ReconciledAbandon { .. }
                )
            } else {
                matches!(effect.state, State::Completed { .. })
            };
            if !allowed {
                return Err(CredentialRebindErrorV1::Blocked("terminal operation"));
            }
            if let Some(stored) = record {
                if !self.intents.is_archived(id) {
                    self.intents.reserve_operation(id)?;
                }
                drop(inspected);
                let effects = self.open_management()?;
                return self.verify_terminal(&effects, &stored, effect.state);
            }
            // A denied Prepared operation has no intent to archive.
            if !matches!(effect.state, State::Denied { .. }) {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            drop(inspected);
            return verify_denied(&self.open_management()?, id);
        }
        // Every branch is selected and proved while inspection still holds its
        // owner lock; normal open can recover only this already joined Started.
        let no_intent_preview = if record.is_none() {
            if !abandon {
                return Err(CredentialRebindErrorV1::Blocked(
                    "no-intent requires explicit abandon",
                ));
            }
            let mut selected = prospective.ok_or(CredentialRebindErrorV1::Blocked(
                "exact candidate re-selection required",
            ))?;
            selected.operation_id = id;
            let current = participation::read_snapshot(&self.state_dir)?;
            let preview = self.refresh_proposal(&current, &selected)?;
            if preview.request_hash != effect.request.request_hash
                || preview.prospective_binding.generation() != effect.request.scope.generation
            {
                return Err(CredentialRebindErrorV1::Stale);
            }
            self.verify_binding(&preview.prospective_binding)?;
            self.intents.reserve_operation(id)?;
            Some(preview)
        } else {
            None
        };
        let current = if let Some(record) = &record {
            if let Some(intent) = record.latest() {
                if abandon {
                    participation::read_snapshot(&self.state_dir)?
                } else {
                    participation::inspect_rebind_publication(
                        &self.state_dir,
                        &self.lease,
                        intent.old_digest,
                        &intent.next,
                    )?
                }
            } else {
                participation::read_snapshot(&self.state_dir)?
            }
        } else {
            participation::read_snapshot(&self.state_dir)?
        };
        if let Some(C08StoredRecordV1::NoIntentAbandon { evidence }) = &record {
            self.intents.reserve_operation(id)?;
            if !abandon || current.exact_digest != evidence.old_digest {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
        }
        if let Some(intent) = record.as_ref().and_then(C08StoredRecordV1::latest) {
            let digest = target_digest(&intent.next)?;
            if current.exact_digest != intent.old_digest && current.exact_digest != Some(digest) {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            if current.exact_digest == intent.old_digest {
                self.verify_binding(&intent.prospective_binding)?;
                if matches!(intent.state, C08IntentStateV1::Completed { .. }) {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
            } else if abandon {
                return Err(CredentialRebindErrorV1::Blocked(
                    "published participation must complete",
                ));
            }
            self.intents.reserve_operation(id)?;
        }
        drop(inspected);
        let effects = self.open_management()?;
        if let Some(preview) = no_intent_preview {
            if let Some(prepared) = effects.prepared_effect(id)? {
                effects.deny(
                    &self.lease,
                    &prepared,
                    ManagementDenialReasonV1::UserDeclined,
                )?;
                return verify_denied(&effects, id);
            }
            let unknown = effects
                .unknown_effect(id)?
                .ok_or(CredentialRebindErrorV1::Corrupt)?;
            let mut evidence = C08NoIntentAbandonEvidenceV1 {
                schema_version: 1,
                operation_id: id,
                management_request: effect.request,
                old_digest: preview.old_digest,
                current_digest: current.exact_digest,
                next: preview.next,
                prospective_binding: preview.prospective_binding,
                confirmation: preview.confirmation,
                c08_inventory_digest: self.intents.inventory_digest(),
                user_confirmed: true,
                evidence_hash: zero_hash()?,
            };
            if !self.intents.operation_absent(id) {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            evidence.evidence_hash = evidence.computed_hash()?;
            let stored = C08StoredRecordV1::NoIntentAbandon {
                evidence: Box::new(evidence),
            };
            self.intents.publish(&stored)?;
            self.checkpoint("no-intent-evidence")?;
            effects.reconcile_abandon(&self.lease, &unknown, stored.evidence_hash()?)?;
            self.verify_terminal(
                &effects,
                &stored,
                State::ReconciledAbandon {
                    evidence_hash: stored.evidence_hash()?,
                },
            )?;
            return Ok(());
        }
        let mut stored = record.ok_or(CredentialRebindErrorV1::Corrupt)?;
        let unknown = effects
            .unknown_effect(id)?
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        if matches!(stored, C08StoredRecordV1::NoIntentAbandon { .. }) {
            effects.reconcile_abandon(&self.lease, &unknown, stored.evidence_hash()?)?;
            return self.verify_terminal(
                &effects,
                &stored,
                State::ReconciledAbandon {
                    evidence_hash: stored.evidence_hash()?,
                },
            );
        }
        let intent = stored
            .latest()
            .ok_or(CredentialRebindErrorV1::Corrupt)?
            .clone();
        if !abandon && matches!(intent.state, C08IntentStateV1::ReconciledAbandon { .. }) {
            return Err(CredentialRebindErrorV1::Blocked("abandoned intent"));
        }
        if abandon {
            if intent.state == C08IntentStateV1::BeforeWrite {
                stored = stored.append(C08IntentStateV1::OutcomeUnknown)?;
                self.intents.publish(&stored)?;
            }
            if !matches!(
                stored.latest().map(|r| &r.state),
                Some(C08IntentStateV1::ReconciledAbandon { .. })
            ) {
                let proof = encoded_hash(
                    b"polaris-c08-intent-abandon-v1\0",
                    &(
                        id,
                        effect.request.request_hash,
                        current.exact_digest,
                        self.intents.inventory_digest(),
                        true,
                    ),
                )?;
                stored = stored.append(C08IntentStateV1::ReconciledAbandon {
                    evidence_hash: proof,
                })?;
                self.intents.publish(&stored)?;
            }
            effects.reconcile_abandon(&self.lease, &unknown, stored.evidence_hash()?)?;
            return self.verify_terminal(
                &effects,
                &stored,
                State::ReconciledAbandon {
                    evidence_hash: stored.evidence_hash()?,
                },
            );
        }
        // Old digest resumes the exact CAS; target digest repairs receipt
        // publication without requiring credentials that may have drifted later.
        let target = target_digest(&intent.next)?;
        let digest = if current.exact_digest == Some(target)
            && participation::read_snapshot(&self.state_dir).is_ok()
        {
            target
        } else {
            participation::publish_v2(
                &self.state_dir,
                &self.lease,
                intent.old_digest,
                &intent.next,
            )?
        };
        if !matches!(
            stored.latest().map(|r| &r.state),
            Some(C08IntentStateV1::Completed { .. })
        ) {
            stored = stored.append(C08IntentStateV1::Completed {
                participation_digest: digest,
            })?;
            self.intents.publish(&stored)?;
        }
        self.checkpoint("intent-completed")?;
        effects.complete_unknown(&self.lease, &unknown, stored.evidence_hash()?)?;
        self.verify_terminal(
            &effects,
            &stored,
            State::Completed {
                evidence_hash: stored.evidence_hash()?,
            },
        )
    }
    fn verify_terminal(
        &mut self,
        effects: &ManagementEffectStoreV1,
        stored: &C08StoredRecordV1,
        state: State,
    ) -> Result<()> {
        self.intents.refresh()?;
        let readback = self
            .intents
            .lookup(stored.operation_id())
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        let effect = effects
            .effect(stored.operation_id())?
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        if readback != stored || effect.state != state {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        validate_pair(&effect, Some(readback), self.project_hash())?;
        if let Some(intent) = stored.latest()
            && let C08IntentStateV1::Completed {
                participation_digest,
            } = intent.state
            && participation::read_snapshot(&self.state_dir)?.exact_digest
                != Some(participation_digest)
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        self.intents.archive_terminal(stored.operation_id())?;
        self.intents.refresh()?;
        if self.intents.lookup(stored.operation_id()) != Some(stored) {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        Ok(())
    }
}
impl PreparedRebindTokenV1 {
    pub fn deny(mut self) -> Result<()> {
        let owner = self.owner.take().ok_or(CredentialRebindErrorV1::Stale)?;
        let inspected = owner.inspect_management(Some(self.preview.operation_id))?;
        drop(inspected);
        let effects = owner.open_management()?;
        effects.deny(
            &owner.lease,
            &self.prepared,
            ManagementDenialReasonV1::UserDeclined,
        )?;
        verify_denied(&effects, self.preview.operation_id)
    }
    #[cfg(unix)]
    pub fn confirm(mut self) -> Result<()> {
        let mut owner = self.owner.take().ok_or(CredentialRebindErrorV1::Stale)?;
        let inspected = owner.inspect_management(Some(self.preview.operation_id))?;
        drop(inspected);
        let effects = owner.open_management()?;
        let checked = (|| {
            owner.intents.refresh()?;
            let current = owner.admission_snapshot()?;
            validate_fresh_invitation(&self.preview)?;
            let expected = owner.refresh_proposal(&current, &self.preview)?;
            if !same_preview(&expected, &self.preview) {
                return Err(CredentialRebindErrorV1::Stale);
            }
            owner.verify_binding(&self.preview.prospective_binding)
        })();
        if let Err(error) = checked {
            effects.deny(
                &owner.lease,
                &self.prepared,
                ManagementDenialReasonV1::StaleRequest,
            )?;
            verify_denied(&effects, self.preview.operation_id)?;
            return Err(error);
        }
        let started = effects.start(&owner.lease, &self.prepared)?;
        let operation = (|| {
            owner.checkpoint("started")?;
            let mut stored = owner.first_intent(&self.preview)?;
            owner.intents.publish(&stored)?;
            owner.checkpoint("before-write")?;
            let digest = participation::publish_v2(
                &owner.state_dir,
                &owner.lease,
                self.preview.old_digest,
                &self.preview.next,
            )?;
            owner.checkpoint("participation-published")?;
            stored = stored.append(C08IntentStateV1::Completed {
                participation_digest: digest,
            })?;
            owner.intents.publish(&stored)?;
            owner.checkpoint("intent-completed")?;
            effects.complete_started(&owner.lease, &started, stored.evidence_hash()?)?;
            owner.checkpoint("management-completed")?;
            owner.verify_terminal(
                &effects,
                &stored,
                State::Completed {
                    evidence_hash: stored.evidence_hash()?,
                },
            )
        })();
        if operation.is_err()
            && effects
                .effect(self.preview.operation_id)?
                .is_some_and(|e| !e.state.is_terminal())
        {
            effects.mark_outcome_unknown(
                &owner.lease,
                &started,
                ManagementUnknownReasonV1::InterruptedAfterStart,
            )?;
        }
        operation
    }
}
fn verify_denied(effects: &ManagementEffectStoreV1, id: Operation) -> Result<()> {
    if !matches!(
        effects.effect(id)?.map(|e| e.state),
        Some(State::Denied { .. })
    ) {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    Ok(())
}
fn proposal(
    project: Hash,
    snapshot: &ParticipationSnapshotV2,
    binding: GitCredentialBindingV1,
    id: Operation,
) -> Result<CredentialRebindPreviewV1> {
    binding.validate()?;
    let (source, previous_generation) = match &snapshot.classification {
        ParticipationReadV2::LegacyParticipationOnly(value) => (value, None),
        ParticipationReadV2::StaleCredentialBindingOnly(value) => {
            (value.inner(), Some(value.credential_binding.generation()))
        }
        _ => {
            return Err(CredentialRebindErrorV1::Blocked(
                "not legacy/stale participation",
            ));
        }
    };
    let old_digest = Some(
        snapshot
            .exact_digest
            .ok_or(CredentialRebindErrorV1::Corrupt)?,
    );
    let remote = source
        .share
        .production_remote()
        .map_err(|_| CredentialRebindErrorV1::Unavailable("HTTPS required"))?;
    if !binding.matches_remote(&remote)
        || previous_generation.map_or(Some(1), |v| v.checked_add(1)) != Some(binding.generation())
    {
        return Err(CredentialRebindErrorV1::Stale);
    }
    let next = ParticipationV2 {
        schema_version: 2,
        participation: source.clone(),
        credential_binding: binding.clone(),
    };
    next.validate()?;
    let confirmation = C08ConfirmationSummaryV1 {
        operation: C08OperationKindV1::Rebind,
        project,
        share_id: source.share.share_id.clone(),
        remote,
        previous_generation,
        next_generation: binding.generation(),
        joined_unix_seconds: source.joined_unix_seconds,
    };
    let request_hash = request_hash(project, old_digest, &next, &confirmation)?;
    Ok(CredentialRebindPreviewV1 {
        operation_id: id,
        old_digest,
        next,
        prospective_binding: binding,
        confirmation,
        request_hash,
    })
}
fn join_proposal(
    project: Hash,
    snapshot: &ParticipationSnapshotV2,
    binding: GitCredentialBindingV1,
    share: super::share::ShareLink,
    manifest: super::share::ShareManifest,
    now: u64,
    id: Operation,
) -> Result<CredentialRebindPreviewV1> {
    binding.validate()?;
    if snapshot.classification != ParticipationReadV2::Missing
        || snapshot.exact_digest.is_some()
        || binding.generation() != 1
    {
        return Err(CredentialRebindErrorV1::Blocked(
            "join requires missing participation",
        ));
    }
    share
        .accept(now)
        .map_err(|_| CredentialRebindErrorV1::Unavailable("invitation expired"))?;
    let remote = share
        .production_remote()
        .map_err(|_| CredentialRebindErrorV1::Unavailable("HTTPS required"))?;
    if !binding.matches_remote(&remote) {
        return Err(CredentialRebindErrorV1::Stale);
    }
    let next = ParticipationV2 {
        schema_version: 2,
        participation: participation::Participation {
            share,
            manifest,
            joined_unix_seconds: now,
            agreed_commit: None,
            applied_commit: None,
            unresolved: Vec::new(),
        },
        credential_binding: binding.clone(),
    };
    next.validate()?;
    let confirmation = C08ConfirmationSummaryV1 {
        operation: C08OperationKindV1::Join,
        project,
        share_id: next.inner().share.share_id.clone(),
        remote,
        previous_generation: None,
        next_generation: 1,
        joined_unix_seconds: now,
    };
    let request_hash = request_hash(project, None, &next, &confirmation)?;
    Ok(CredentialRebindPreviewV1 {
        operation_id: id,
        old_digest: None,
        next,
        prospective_binding: binding,
        confirmation,
        request_hash,
    })
}
fn same_preview(a: &CredentialRebindPreviewV1, b: &CredentialRebindPreviewV1) -> bool {
    a.operation_id == b.operation_id
        && a.old_digest == b.old_digest
        && a.next == b.next
        && a.prospective_binding == b.prospective_binding
        && a.confirmation == b.confirmation
        && a.request_hash == b.request_hash
}
fn validate_pair(
    effect: &ManagementEffectSnapshotV1,
    record: Option<&C08StoredRecordV1>,
    project: Hash,
) -> Result<()> {
    if effect.request.inventory.encoded() != "C08"
        || effect.request.scope.project != project
        || effect.request.scope.session.is_some()
        || effect.request.scope.permission != ManagementPermissionScopeV1::Project
        || effect.request.scope.account_binding.is_some()
        || effect.request.rewind != ManagementRewindDispositionV1::IrreversibleLocal
    {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    let Some(record) = record else {
        return if matches!(
            effect.state,
            State::Prepared
                | State::EffectStarted
                | State::OutcomeUnknown { .. }
                | State::Denied { .. }
        ) {
            Ok(())
        } else {
            Err(CredentialRebindErrorV1::Corrupt)
        };
    };
    record.canonical_bytes()?;
    if record.operation_id() != effect.request.operation_id {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    let valid = match record {
        C08StoredRecordV1::NoIntentAbandon { evidence } => {
            evidence.management_request == effect.request
                && match &effect.state {
                    State::OutcomeUnknown { .. } => true,
                    State::ReconciledAbandon { evidence_hash } => {
                        *evidence_hash == record.evidence_hash()?
                    }
                    _ => false,
                }
        }
        C08StoredRecordV1::IntentHistory { records } => {
            let intent = records.last().ok_or(CredentialRebindErrorV1::Corrupt)?;
            intent.request_hash == effect.request.request_hash
                && intent.project == project
                && intent.prospective_binding.generation() == effect.request.scope.generation
                && match (&effect.state, &intent.state) {
                    (
                        State::EffectStarted | State::OutcomeUnknown { .. },
                        C08IntentStateV1::BeforeWrite
                        | C08IntentStateV1::OutcomeUnknown
                        | C08IntentStateV1::Completed { .. },
                    ) => true,
                    (State::OutcomeUnknown { .. }, C08IntentStateV1::ReconciledAbandon { .. }) => {
                        true
                    }
                    (State::Completed { evidence_hash }, C08IntentStateV1::Completed { .. })
                    | (
                        State::ReconciledAbandon { evidence_hash },
                        C08IntentStateV1::ReconciledAbandon { .. },
                    ) => *evidence_hash == record.evidence_hash()?,
                    _ => false,
                }
        }
    };
    if valid {
        Ok(())
    } else {
        Err(CredentialRebindErrorV1::Corrupt)
    }
}
fn request_hash(
    project: Hash,
    old: Option<SyncDigestV1>,
    next: &ParticipationV2,
    confirmation: &C08ConfirmationSummaryV1,
) -> Result<Hash> {
    next.validate()?;
    if project != confirmation.project
        || confirmation.share_id != next.inner().share.share_id
        || confirmation.remote
            != next
                .inner()
                .share
                .production_remote()
                .map_err(|_| CredentialRebindErrorV1::Corrupt)?
        || confirmation.next_generation != next.credential_binding.generation()
        || confirmation.joined_unix_seconds != next.inner().joined_unix_seconds
        || confirmation
            .previous_generation
            .map_or(Some(1), |v| v.checked_add(1))
            != Some(confirmation.next_generation)
        || (confirmation.operation == C08OperationKindV1::Join) != old.is_none()
        || (confirmation.operation == C08OperationKindV1::Join
            && confirmation.previous_generation.is_some())
    {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    encoded_hash(
        b"polaris-c08-request-v1\0",
        &(
            project,
            &next.inner().share.share_id,
            old,
            next,
            &next.credential_binding,
            confirmation,
        ),
    )
}
fn encoded_hash(domain: &[u8], value: &impl Serialize) -> Result<Hash> {
    let mut bytes = domain.to_vec();
    bytes.extend_from_slice(
        &serde_json::to_vec(value).map_err(|_| CredentialRebindErrorV1::Corrupt)?,
    );
    if bytes.len() > MAX_RECORD {
        return Err(CredentialRebindErrorV1::Blocked("record capacity"));
    }
    Ok(Hash::digest(&bytes))
}
fn target_digest(next: &ParticipationV2) -> Result<SyncDigestV1> {
    Ok(SyncDigestV1::digest(&next.canonical_bytes()?))
}
fn zero_hash() -> Result<Hash> {
    Hash::from_hex(&"0".repeat(64)).map_err(|_| CredentialRebindErrorV1::Corrupt)
}
#[cfg(unix)]
fn reject_legacy_remote_intents(state: &Path) -> Result<()> {
    // V1 lacks this owner's authorization. Even a readable legacy intent is
    // not promoted into a C08 bypass by inspecting its old terminal flag.
    for name in ["remote-intents-v1"] {
        match fs::symlink_metadata(state.join("sync").join(name)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
            Ok(_) => return Err(CredentialRebindErrorV1::Blocked("C10 intent present")),
        }
    }
    Ok(())
}
#[cfg(all(test, unix))]
fn test_stored_record(id: Operation, terminal: bool) -> C08StoredRecordV1 {
    tests::stored_record(id, terminal)
}
#[cfg(all(test, unix))]
fn test_extend_stored_record(prior: &C08StoredRecordV1, terminal: bool) -> C08StoredRecordV1 {
    prior
        .append(if terminal {
            C08IntentStateV1::Completed {
                participation_digest: target_digest(&prior.latest().unwrap().next).unwrap(),
            }
        } else {
            C08IntentStateV1::OutcomeUnknown
        })
        .unwrap()
}

#[cfg(all(test, unix))]
fn test_no_intent_record(id: Operation) -> C08StoredRecordV1 {
    tests::no_intent_record(id)
}

#[cfg(test)]
thread_local! {
    static TEST_MANAGEMENT_BUDGET: std::cell::Cell<Option<(u64, u64, u64)>> = const { std::cell::Cell::new(None) };
}
fn check_management_capacity(budget: (u64, u64, u64)) -> Result<()> {
    #[cfg(test)]
    let budget =
        TEST_MANAGEMENT_BUDGET.with(|override_budget| override_budget.get().unwrap_or(budget));
    let (record, remaining, bytes) = budget;
    if record == 0 || remaining < 4 || record.checked_mul(4).is_none_or(|v| bytes < v) {
        return Err(CredentialRebindErrorV1::Blocked("management capacity"));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static TEST_NOW: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}
fn trusted_now() -> Result<u64> {
    #[cfg(test)]
    if let Some(now) = TEST_NOW.with(|value| value.get()) {
        return Ok(now);
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| CredentialRebindErrorV1::Unavailable("clock"))
}

fn validate_fresh_invitation(preview: &CredentialRebindPreviewV1) -> Result<()> {
    if preview.confirmation.operation == C08OperationKindV1::Join {
        preview
            .next
            .inner()
            .share
            .accept(trusted_now()?)
            .map_err(|_| CredentialRebindErrorV1::Unavailable("invitation expired"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn unavailable() -> CredentialRebindErrorV1 {
    CredentialRebindErrorV1::Unavailable("credential rebind is unavailable on this platform")
}

#[cfg(not(unix))]
impl<'a> C08IdleReadV1<'a> {
    pub(crate) fn inspect_with_read_lease(
        _project: &Path,
        _state: &Path,
        _lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    ) -> Result<Self> {
        Err(unavailable())
    }

    pub(crate) fn require_idle(&mut self) -> Result<Hash> {
        Err(unavailable())
    }
}

#[cfg(not(unix))]
impl CredentialRebindOwnerV1 {
    fn acquire_inner(_project_root: &Path, _credentials: BindingVerifier) -> Result<Self> {
        Err(unavailable())
    }

    fn inspect_management(&self, _own: Option<Operation>) -> Result<ManagementEffectInspectOpenV1> {
        Err(unavailable())
    }
}

#[cfg(not(unix))]
impl CredentialRebindOwnerV1 {
    fn recover_or_abandon(
        self,
        _id: Operation,
        _prospective: Option<CredentialRebindPreviewV1>,
        _abandon: bool,
    ) -> Result<()> {
        Err(unavailable())
    }
}

#[cfg(not(unix))]
impl PreparedRebindTokenV1 {
    pub fn confirm(self) -> Result<()> {
        Err(unavailable())
    }
}
