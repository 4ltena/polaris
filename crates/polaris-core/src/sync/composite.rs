//! Read-only project admission across retained C08/C10 and management evidence.
//! Callers keep their actual EX or SH; a returned snapshot is never a grant.
#[cfg(unix)]
use super::{
    credential_rebind::C08IdleReadV1,
    owner::{C10IntentReadV2, C10PendingIntentV2},
    participation,
    remote_intent::{SyncDigestV1, SyncRemoteIntentStateV2},
};
#[cfg(unix)]
use crate::{
    management_effect::ManagementEffectStateV1 as State, project_mutation::ProjectMutationOwnerV1,
};
use crate::{
    management_effect::{ManagementEffectInspectOpenV1, ManagementEffectSnapshotV1},
    project_mutation::{ProjectActivityLeaseV1, ProjectMutationLeaseV1, ProjectReadLeaseV1},
};
use std::path::Path;
#[cfg(unix)]
use std::{collections::BTreeSet, fs};

#[derive(Debug, thiserror::Error)]
pub enum C10CompositeErrorV1 {
    #[error(
        "Unresolved operations exist. Inspect state, then reconcile or explicitly abandon them"
    )]
    Blocked,
    #[error("Cannot verify management and sync record consistency")]
    Invalid,
    #[error("Cannot inspect state while an operation is active")]
    Busy,
}
type Result<T> = std::result::Result<T, C10CompositeErrorV1>;

pub struct C10CompositeBlockerV1;
impl C10CompositeBlockerV1 {
    pub fn inspect_with_mutation(
        project: &Path,
        state: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        Self::inspect(project, state, ProjectReadLeaseV1::Mutation(lease))
    }

    pub fn inspect_with_activity(
        project: &Path,
        state: &Path,
        lease: &ProjectActivityLeaseV1,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        Self::inspect(project, state, ProjectReadLeaseV1::Activity(lease))
    }

    /// Informational snapshot only. Open existing locks without initialization.
    #[cfg(unix)]
    pub fn inspect_readonly(
        project: &Path,
        state: &Path,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        let owner = ProjectMutationOwnerV1::open_existing(&state.join("rewind-workspace"))
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
        let activity = owner.try_activity().map_err(|error| match error {
            crate::project_mutation::ProjectMutationError::Busy => C10CompositeErrorV1::Busy,
            _ => C10CompositeErrorV1::Invalid,
        })?;
        Self::inspect_with_activity(project, state, &activity)
    }

    #[cfg(not(unix))]
    pub fn inspect_readonly(
        _project: &Path,
        _state: &Path,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        Err(C10CompositeErrorV1::Invalid)
    }

    pub fn require_clear_with_mutation(
        project: &Path,
        state: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<()> {
        match Self::inspect_with_mutation(project, state, lease)? {
            None => Ok(()),
            Some(_) => Err(C10CompositeErrorV1::Blocked),
        }
    }

    #[cfg(unix)]
    fn inspect(
        project: &Path,
        state: &Path,
        lease: ProjectReadLeaseV1<'_>,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        let project = canonical_project(project)?;
        require_project_state(&project, state, lease)?;
        lease
            .verify(state)
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
        crate::apply_intent::ApplyIntentIdleReadV1::inspect(state, lease)
            .and_then(|v| v.require_idle())
            .map_err(|_| C10CompositeErrorV1::Blocked)?;
        let mut c08 = C08IdleReadV1::inspect_with_read_lease(&project, state, lease)
            .map_err(|_| C10CompositeErrorV1::Blocked)?;
        let part = participation::read_snapshot(state).map_err(|_| C10CompositeErrorV1::Invalid)?;
        let blocker = inspect_c10_pairs(&project, state, lease, c08.management())?;
        let blocker = blocker.or_else(|| match c08.management() {
            ManagementEffectInspectOpenV1::Absent => None,
            ManagementEffectInspectOpenV1::Present(value) => {
                value.effects().find(|e| !e.state.is_terminal()).cloned()
            }
        });
        c08.require_idle()
            .map_err(|_| C10CompositeErrorV1::Blocked)?;
        if participation::read_snapshot(state).map_err(|_| C10CompositeErrorV1::Invalid)? != part {
            return Err(C10CompositeErrorV1::Invalid);
        }
        lease
            .verify(state)
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
        require_project_state(&project, state, lease)?;
        Ok(blocker)
    }

    #[cfg(not(any(unix, all(test, windows))))]
    fn inspect(
        _project: &Path,
        _state: &Path,
        _lease: ProjectReadLeaseV1<'_>,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        Err(C10CompositeErrorV1::Invalid)
    }

    #[cfg(all(test, windows))]
    fn inspect(
        project: &Path,
        state: &Path,
        lease: ProjectReadLeaseV1<'_>,
    ) -> Result<Option<ManagementEffectSnapshotV1>> {
        use polaris_auth::protection::{WindowsFamilyDirectory, WindowsPinnedDirectory};
        lease.mutation().map_err(windows_invalid)?;
        let project_pin =
            WindowsPinnedDirectory::open_existing(project).map_err(windows_invalid)?;
        let project = std::fs::canonicalize(project).map_err(windows_invalid)?;
        let named_project =
            WindowsPinnedDirectory::open_existing(&project).map_err(windows_invalid)?;
        if project_pin.identity() != named_project.identity() {
            return Err(C10CompositeErrorV1::Invalid);
        }
        let state_pin = WindowsFamilyDirectory::open_existing(state).map_err(windows_invalid)?;
        let storage = crate::project::storage_root_from_env().map_err(windows_invalid)?;
        let storage_pin =
            WindowsFamilyDirectory::open_existing(&storage).map_err(windows_invalid)?;
        let states = storage_pin
            .child(std::ffi::OsStr::new("state"), false)
            .map_err(windows_invalid)?;
        let expected = states
            .child(
                std::ffi::OsStr::new(&crate::project::project_id(&project)),
                false,
            )
            .map_err(windows_invalid)?;
        if expected.identity() != state_pin.identity() {
            return Err(C10CompositeErrorV1::Invalid);
        }
        lease.verify(state).map_err(windows_invalid)?;
        windows_require_absent(&state_pin)?;
        let management =
            crate::management_effect::ManagementEffectStoreV1::inspect_open_with_read_lease(
                &project, state, lease,
            )
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
        let blocker = match &management {
            ManagementEffectInspectOpenV1::Absent => None,
            ManagementEffectInspectOpenV1::Present(value) => {
                value.verify_read_snapshot().map_err(windows_invalid)?;
                if value.effects().any(|effect| {
                    matches!(
                        effect.request.inventory.encoded().as_str(),
                        "C08" | "C10" | "C12" | "D27"
                    )
                }) {
                    return Err(C10CompositeErrorV1::Blocked);
                }
                value
                    .effects()
                    .find(|effect| !effect.state.is_terminal())
                    .cloned()
            }
        };
        windows_require_absent(&state_pin)?;
        if let ManagementEffectInspectOpenV1::Present(value) = &management {
            value.verify_read_snapshot().map_err(windows_invalid)?;
        }
        expected.revalidate().map_err(windows_invalid)?;
        state_pin.revalidate().map_err(windows_invalid)?;
        project_pin.revalidate().map_err(windows_invalid)?;
        named_project.revalidate().map_err(windows_invalid)?;
        lease.verify(state).map_err(windows_invalid)?;
        Ok(blocker)
    }
}

#[cfg(all(test, windows))]
fn windows_invalid<E>(_: E) -> C10CompositeErrorV1 {
    C10CompositeErrorV1::Invalid
}

#[cfg(all(test, windows))]
fn windows_require_absent(state: &polaris_auth::protection::WindowsFamilyDirectory) -> Result<()> {
    // Direct native lookup also closes case-folding and file-vs-directory aliases.
    for name in ["sync", "apply-intents-v1"] {
        match state.child(std::ffi::OsStr::new(name), false) {
            Err(polaris_auth::AuthError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(C10CompositeErrorV1::Blocked),
        }
    }
    state.revalidate().map_err(|_| C10CompositeErrorV1::Invalid)
}

#[cfg(unix)]
fn require_project_state(
    project: &Path,
    state: &Path,
    lease: ProjectReadLeaseV1<'_>,
) -> Result<()> {
    lease
        .verify(state)
        .map_err(|_| C10CompositeErrorV1::Invalid)?;
    match fs::symlink_metadata(lease.store_root().join("namespace.json")) {
        Ok(_) => {
            // Explicit private state roots are already supported by the rewind
            // owner. Reuse its actual project/device/inode namespace proof.
            crate::rewind_workspace::WorkspaceOwner::open_existing_read_only(
                project,
                lease.store_root(),
                Default::default(),
            )
            .map_err(|_| C10CompositeErrorV1::Invalid)?
            .ok_or(C10CompositeErrorV1::Invalid)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A newly acquired project lease can precede rewind initialization.
            // Calculate the existing state location without creating anything.
            let expected = crate::project::storage_root_from_env()
                .map_err(|_| C10CompositeErrorV1::Invalid)?
                .join("state")
                .join(crate::project::project_id(project));
            if fs::canonicalize(expected).map_err(|_| C10CompositeErrorV1::Invalid)? != state {
                return Err(C10CompositeErrorV1::Invalid);
            }
        }
        Err(_) => return Err(C10CompositeErrorV1::Invalid),
    }
    Ok(())
}

#[cfg(unix)]
fn canonical_project(project: &Path) -> Result<std::path::PathBuf> {
    fs::canonicalize(project).map_err(|_| C10CompositeErrorV1::Invalid)
}

/// C08 retains its own intent lock and recovery exception. It reuses this
/// C10-only pair check with its already verified management snapshot, avoiding
/// another C08 or management owner acquisition.
#[cfg(unix)]
pub(crate) fn inspect_c10_pairs(
    project: &Path,
    state: &Path,
    lease: ProjectReadLeaseV1<'_>,
    management: &ManagementEffectInspectOpenV1,
) -> Result<Option<ManagementEffectSnapshotV1>> {
    let project = canonical_project(project)?;
    require_project_state(&project, state, lease)?;
    let project_hash = SyncDigestV1::digest(project.as_os_str().as_encoded_bytes());
    lease
        .verify(state)
        .map_err(|_| C10CompositeErrorV1::Invalid)?;
    let effects: Vec<_> = match management {
        ManagementEffectInspectOpenV1::Absent => Vec::new(),
        ManagementEffectInspectOpenV1::Present(v) => {
            v.verify_read_snapshot()
                .map_err(|_| C10CompositeErrorV1::Invalid)?;
            v.effects().cloned().collect()
        }
    };
    // A missing/corrupt participation never becomes an implicit repair.
    let part = participation::read_snapshot(state).map_err(|_| C10CompositeErrorV1::Invalid)?;
    let store = C10IntentReadV2::inspect(state, lease).map_err(|_| C10CompositeErrorV1::Invalid)?;
    if !store
        .legacy_diagnostics()
        .map_err(|_| C10CompositeErrorV1::Invalid)?
        .is_empty()
        || store.pending() != &C10PendingIntentV2::None
    {
        return Err(C10CompositeErrorV1::Blocked);
    }
    let archive = store
        .archive_inventory_digest()
        .map_err(|_| C10CompositeErrorV1::Invalid)?;
    let mut paired = BTreeSet::new();
    let mut blocker = None;
    for history in store.active_records() {
        let effect = effects
            .iter()
            .find(|v| v.request.operation_id == history.operation_id())
            .ok_or(C10CompositeErrorV1::Invalid)?;
        super::push::validate_pair_identity(history, effect, project_hash)
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
        match history.current().state() {
            SyncRemoteIntentStateV2::Completed { .. }
            | SyncRemoteIntentStateV2::ReconciledAbandon { .. } => {
                store
                    .active_durable_terminal_pair_readback(history.operation_id(), effect)
                    .map_err(|_| C10CompositeErrorV1::Blocked)?;
            }
            _ if matches!(
                effect.state,
                State::EffectStarted | State::OutcomeUnknown { .. }
            ) =>
            {
                if blocker.is_none() {
                    blocker = Some(effect.clone());
                }
            }
            _ => return Err(C10CompositeErrorV1::Invalid),
        }
        if !paired.insert(history.operation_id()) {
            return Err(C10CompositeErrorV1::Invalid);
        }
    }
    let mut cursor = None;
    loop {
        let page = store
            .archive_page(cursor.as_ref())
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
        for history in page.records {
            let effect = effects
                .iter()
                .find(|v| v.request.operation_id == history.operation_id())
                .ok_or(C10CompositeErrorV1::Invalid)?;
            if history.current().identity().project != project_hash
                || !paired.insert(history.operation_id())
            {
                return Err(C10CompositeErrorV1::Invalid);
            }
            store
                .archived_terminal_pair_readback(history.operation_id(), effect)
                .map_err(|_| C10CompositeErrorV1::Invalid)?;
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    for effect in effects
        .iter()
        .filter(|e| e.request.inventory.encoded() == "C10")
    {
        if !paired.contains(&effect.request.operation_id) {
            match effect.state {
                State::Denied { .. } => {}
                State::Prepared | State::EffectStarted | State::OutcomeUnknown { .. } => {
                    if blocker.is_none() {
                        blocker = Some(effect.clone());
                    }
                }
                _ => return Err(C10CompositeErrorV1::Invalid),
            }
        }
    }
    store
        .verify_read_inventory(archive)
        .map_err(|_| C10CompositeErrorV1::Invalid)?;
    if participation::read_snapshot(state).map_err(|_| C10CompositeErrorV1::Invalid)? != part {
        return Err(C10CompositeErrorV1::Invalid);
    }
    if let ManagementEffectInspectOpenV1::Present(v) = management {
        v.verify_read_snapshot()
            .map_err(|_| C10CompositeErrorV1::Invalid)?;
    }
    lease
        .verify(state)
        .map_err(|_| C10CompositeErrorV1::Invalid)?;
    require_project_state(&project, state, lease)?;
    Ok(blocker)
}

#[cfg(not(unix))]
pub(crate) fn inspect_c10_pairs(
    _project: &Path,
    _state: &Path,
    _lease: ProjectReadLeaseV1<'_>,
    _management: &ManagementEffectInspectOpenV1,
) -> Result<Option<ManagementEffectSnapshotV1>> {
    Err(C10CompositeErrorV1::Invalid)
}

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;
