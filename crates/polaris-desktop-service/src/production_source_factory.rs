//! Trusted source recovery preparation; never source dispatch authority.
use crate::{
    ConfirmedSourceGrant, CurrentSourcePolicy, PreparedSourceRecovery, ServiceError,
    SourceApplyRequest, TrustedRunCompletion, TrustedSourceFactory,
};
use polaris_core::{
    desktop_store::{BootstrapTier, Published},
    workspace_apply::RecoveryParent,
};
use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, PathBuf},
    sync::Arc,
};

/// Owns only the supplied trusted base. Never discovers, creates or reopens it.
/// Failed preparation may leave a directory; no automatic cleanup or reuse.
pub struct ProductionSourceFactory {
    grant: Arc<ConfirmedSourceGrant>,
    base: File,
    pinned: RecoveryParent,
}
impl ProductionSourceFactory {
    pub fn new(
        grant: Arc<ConfirmedSourceGrant>,
        recovery_base: File,
        expected_identity: (u64, u64),
        provenance: PathBuf,
    ) -> Result<Self, ServiceError> {
        if !provenance.is_absolute()
            || provenance.to_str().is_none()
            || provenance.as_os_str().len() > 4000
            || provenance
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            return Err(ServiceError::Options);
        }
        let pinned = RecoveryParent::pin(&recovery_base, expected_identity, &provenance)
            .map_err(|_| ServiceError::Options)?;
        Ok(Self {
            grant,
            base: recovery_base,
            pinned,
        })
    }
    fn check(
        &self,
        completion: &TrustedRunCompletion,
        published: &Published,
        request: &SourceApplyRequest,
    ) -> Result<(), ServiceError> {
        let policy = &self.grant.policy;
        let snapshot = completion.prepared().snapshot();
        let terminal = completion.terminal();
        if published.marker.deleted
            || &published.marker.project_id != completion.project_id()
            || &published.marker.session_id != completion.session_id()
            || terminal.run.run_id != completion.target().run_id
            || terminal.run.attempt_id != completion.target().attempt_id
            || !terminal.run.state.is_terminal()
            || terminal.result_id.is_none()
            || !published.state.runs.iter().any(|run| run == terminal)
            || published.marker.session_revision != request.expected_session_revision
            || published.marker.session_revision < completion.session_revision()
            || published.state.policy_revision != policy.policy_revision
            || terminal.policy_revision != policy.policy_revision
            || !policy.read_allowed
            || !policy.write_allowed
            || self.grant.tier == BootstrapTier::ReadOnly
            || policy.source_path != snapshot.source_path
            || policy.source_identity.device.get() != snapshot.source_identity.device
            || policy.source_identity.inode.get() != snapshot.source_identity.inode
        {
            return Err(ServiceError::Options);
        }
        // Metadata only: source identity, actual ancestry and same filesystem.
        // Do this on the base BEFORE any mkdir, then again on the created child.
        self.pinned
            .validate_for_snapshot(snapshot)
            .map_err(|_| ServiceError::Options)
    }
}
impl ProductionSourceFactory {
    fn prepare_recovery(
        &self,
        completion: &TrustedRunCompletion,
        published: &Published,
        request: &SourceApplyRequest,
        #[cfg(test)] after_mkdir: impl FnOnce(&File, &str),
    ) -> Result<(CurrentSourcePolicy, PreparedSourceRecovery), ServiceError> {
        self.check(completion, published, request)?;
        let tuple = serde_json::to_vec(&(
            "source-recovery-v1",
            completion.project_id(),
            completion.session_id(),
            &completion.target().run_id,
            &completion.target().attempt_id,
            &request.operation_id,
        ))
        .map_err(|_| ServiceError::Options)?;
        let name = format!(
            "apply-{}",
            polaris_core::conversation_state::content_hash(&tuple)
        );
        let component = CString::new(name.as_bytes()).map_err(|_| ServiceError::Options)?;
        // SAFETY: owned directory FD and one bounded, NUL-free component.
        // EEXIST is an error, including a directory left by earlier failure.
        if unsafe { libc::mkdirat(self.base.as_raw_fd(), component.as_ptr(), 0o700) } != 0 {
            return Err(ServiceError::Options);
        }
        #[cfg(test)]
        after_mkdir(&self.base, &name);
        // Every error below retains the created directory. No path-based retry.
        // SAFETY: same owned base FD; no symlink or inherited-descriptor fallback.
        let fd = unsafe {
            libc::openat(
                self.base.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(ServiceError::Options);
        }
        // SAFETY: successful openat returned a newly owned FD.
        let child = unsafe { File::from_raw_fd(fd) };
        let metadata = child.metadata().map_err(|_| ServiceError::Options)?;
        let path = self.pinned.provenance().join(name);
        let parent = RecoveryParent::pin(&child, (metadata.dev(), metadata.ino()), &path)
            .map_err(|_| ServiceError::Options)?;
        parent
            .validate_for_snapshot(completion.prepared().snapshot())
            .map_err(|_| ServiceError::Options)?;
        child.sync_all().map_err(|_| ServiceError::Options)?;
        self.base.sync_all().map_err(|_| ServiceError::Options)?;
        self.check(completion, published, request)?;
        parent
            .validate_for_snapshot(completion.prepared().snapshot())
            .map_err(|_| ServiceError::Options)?;
        Ok((
            self.grant.policy.clone(),
            PreparedSourceRecovery { parent, path },
        ))
    }
}

impl TrustedSourceFactory for ProductionSourceFactory {
    fn prepare(
        &self,
        completion: &TrustedRunCompletion,
        published: &Published,
        request: &SourceApplyRequest,
    ) -> Result<(CurrentSourcePolicy, PreparedSourceRecovery), ServiceError> {
        self.prepare_recovery(
            completion,
            published,
            request,
            #[cfg(test)]
            |_, _| {},
        )
    }
}

#[cfg(test)]
mod tests;
