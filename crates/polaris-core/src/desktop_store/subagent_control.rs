//! Durable, private owner storage for the typed subagent control plane.
//!
//! This is deliberately separate from the session sidecar.  A control-plane
//! update can be frequent, while a sidecar revision is a conversation
//! publication boundary.  The owner holds a private advisory lock, writes a
//! complete next record, syncs it, and atomically replaces the current record.
//! No child process, provider, or tool capability is represented here.

use super::{StoreError, StoreResult, disk::Directory};
use crate::{
    conversation_state::content_hash,
    subagent_control::{ChildControlPlaneV1, RootAttemptId, RootRunId, TaskId},
};
use polaris_desktop_protocol::ids::{ProjectId, SessionId};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

const ROOT_DIRECTORY: &str = "subagent-control-v1";
const STATE_FILE: &str = "state.json";
const NEXT_FILE: &str = "state.next";
const LOCK_FILE: &str = "owner.lock";
const SCHEMA_VERSION: u8 = 1;
const MAX_STATE_BYTES: usize = 256 * 1024;

/// Immutable coordinates accepted when the owner is opened.  Every persisted
/// child must stay within these coordinates; a restored plane can therefore
/// never be attached to a different session, root run, or active task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentControlBindingV1 {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub root_run_id: RootRunId,
    pub root_attempt_id: RootAttemptId,
    pub active_task_id: TaskId,
}

/// A read-only copy returned at an explicit durable revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentControlSnapshotV1 {
    pub revision: u64,
    pub binding: SubagentControlBindingV1,
    pub control_plane: ChildControlPlaneV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredControlPlaneV1 {
    schema_version: u8,
    revision: u64,
    binding: SubagentControlBindingV1,
    control_plane: ChildControlPlaneV1,
}

/// Single private owner for one session/root-run/task control plane.
///
/// `replace` is compare-and-swap guarded even though the owner also holds an
/// OS lock.  The revision protects callers from applying a stale UI/protocol
/// snapshot after a reload or recovery path.
#[derive(Debug)]
pub struct SubagentControlOwner {
    directory: Arc<Directory>,
    guard: File,
    stored: StoredControlPlaneV1,
}

impl SubagentControlOwner {
    /// Open the exact private root, then create or load one bound state.
    /// The root must be an already-owned, canonical desktop-storage root.
    pub fn open_or_create_owned(
        root: &Path,
        binding: SubagentControlBindingV1,
    ) -> StoreResult<Self> {
        let root = Arc::new(Directory::open_owned(root)?);
        Self::from_root(root, binding)
    }

    pub(super) fn from_root(
        root: Arc<Directory>,
        binding: SubagentControlBindingV1,
    ) -> StoreResult<Self> {
        validate_binding(&binding)?;
        let controls = root.ensure_child(ROOT_DIRECTORY)?;
        let project = controls.ensure_child(&bucket(binding.project_id.as_str()))?;
        let session = project.ensure_child(&bucket(binding.session_id.as_str()))?;
        let root_run = session.ensure_child(&bucket(&binding.root_run_id.0))?;
        let root_attempt = root_run.ensure_child(&bucket(&binding.root_attempt_id.0))?;
        let directory = Arc::new(root_attempt.ensure_child(&bucket(&binding.active_task_id.0))?);
        let guard = open_lock(&directory)?;
        acquire(&guard)?;

        let stored = match read_stored(&directory) {
            Ok(stored) => {
                validate_stored(&stored)?;
                if stored.binding != binding {
                    return Err(StoreError::TargetMismatch);
                }
                stored
            }
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                let stored = StoredControlPlaneV1 {
                    schema_version: SCHEMA_VERSION,
                    revision: 0,
                    binding,
                    control_plane: ChildControlPlaneV1::default(),
                };
                validate_stored(&stored)?;
                create_initial(&directory, &stored)?;
                stored
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            directory,
            guard,
            stored,
        })
    }

    pub fn snapshot(&self) -> StoreResult<SubagentControlSnapshotV1> {
        self.ready()?;
        Ok(snapshot_of(&self.stored))
    }

    /// Reload the durable value after validating the private lock and binding.
    pub fn reload(&mut self) -> StoreResult<SubagentControlSnapshotV1> {
        self.ready()?;
        let stored = read_stored(&self.directory)?;
        validate_stored(&stored)?;
        if stored.binding != self.stored.binding {
            return Err(StoreError::TargetMismatch);
        }
        self.stored = stored;
        Ok(snapshot_of(&self.stored))
    }

    /// Atomically replace the control plane at exactly `expected_revision`.
    /// A byte-identical replacement is an idempotent read and retains the
    /// current revision; every material update advances it once.
    pub fn replace(
        &mut self,
        expected_revision: u64,
        control_plane: ChildControlPlaneV1,
    ) -> StoreResult<SubagentControlSnapshotV1> {
        self.ready()?;
        let current = read_stored(&self.directory)?;
        validate_stored(&current)?;
        if current.binding != self.stored.binding {
            return Err(StoreError::TargetMismatch);
        }
        if current.revision != expected_revision {
            return Err(StoreError::CasConflict("subagent control"));
        }
        if current.control_plane == control_plane {
            self.stored = current;
            return Ok(snapshot_of(&self.stored));
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(StoreError::Overflow)?;
        let next = StoredControlPlaneV1 {
            schema_version: SCHEMA_VERSION,
            revision,
            binding: current.binding,
            control_plane,
        };
        validate_stored(&next)?;
        publish(&self.directory, &next)?;
        self.stored = next;
        Ok(snapshot_of(&self.stored))
    }

    fn ready(&self) -> StoreResult<()> {
        self.directory.verify()?;
        let lock = self.directory.open(LOCK_FILE, false, false)?;
        super::disk::same(&self.guard, &lock)
    }
}

fn snapshot_of(stored: &StoredControlPlaneV1) -> SubagentControlSnapshotV1 {
    SubagentControlSnapshotV1 {
        revision: stored.revision,
        binding: stored.binding.clone(),
        control_plane: stored.control_plane.clone(),
    }
}

fn validate_stored(stored: &StoredControlPlaneV1) -> StoreResult<()> {
    if stored.schema_version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedVersion(stored.schema_version.into()));
    }
    validate_binding(&stored.binding)?;
    stored
        .control_plane
        .validate()
        .map_err(|_| StoreError::Corrupt("subagent control plane"))?;
    if stored.control_plane.children.iter().any(|child| {
        child.binding.root_run_id != stored.binding.root_run_id
            || child.binding.root_attempt_id != stored.binding.root_attempt_id
            || child.binding.active_task_id != stored.binding.active_task_id
    }) {
        return Err(StoreError::TargetMismatch);
    }
    Ok(())
}

fn validate_binding(binding: &SubagentControlBindingV1) -> StoreResult<()> {
    // The typed IDs intentionally remain transparent for serde.  Validate the
    // persisted strings again at this security boundary rather than relying on
    // a constructor having been used before deserialization.
    for value in [
        &binding.root_run_id.0,
        &binding.root_attempt_id.0,
        &binding.active_task_id.0,
    ] {
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err(StoreError::Corrupt("subagent control binding"));
        }
    }
    Ok(())
}

fn bucket(value: &str) -> String {
    content_hash(value.as_bytes())
}

fn open_lock(directory: &Directory) -> StoreResult<File> {
    match directory.open(LOCK_FILE, true, true) {
        Ok(file) => Ok(file),
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            directory.open(LOCK_FILE, true, false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn acquire(file: &File) -> StoreResult<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        Err(StoreError::Busy)
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(unix))]
fn acquire(_: &File) -> StoreResult<()> {
    Err(StoreError::UnsupportedPlatform)
}

fn read_stored(directory: &Directory) -> StoreResult<StoredControlPlaneV1> {
    let mut file = directory.open(STATE_FILE, false, false)?;
    let length = file.metadata()?.len();
    if length == 0 || length > MAX_STATE_BYTES as u64 {
        return Err(StoreError::Corrupt("subagent control state size"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length || bytes.len() > MAX_STATE_BYTES {
        return Err(StoreError::Corrupt("subagent control state size"));
    }
    serde_json::from_slice(&bytes).map_err(StoreError::from)
}

fn create_initial(directory: &Directory, stored: &StoredControlPlaneV1) -> StoreResult<()> {
    let bytes = encoded(stored)?;
    let mut file = match directory.open(STATE_FILE, true, true) {
        Ok(file) => file,
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_stored(directory)?;
            validate_stored(&existing)?;
            if existing == *stored {
                return Ok(());
            }
            return Err(StoreError::TargetMismatch);
        }
        Err(error) => return Err(error),
    };
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    directory.sync()
}

fn publish(directory: &Directory, stored: &StoredControlPlaneV1) -> StoreResult<()> {
    let bytes = encoded(stored)?;
    directory.remove_next()?;
    let mut file = directory.open(NEXT_FILE, true, true)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    directory.publish_next()?;
    directory.sync()
}

fn encoded(stored: &StoredControlPlaneV1) -> StoreResult<Vec<u8>> {
    let bytes = serde_json::to_vec(stored)?;
    if bytes.is_empty() || bytes.len() > MAX_STATE_BYTES {
        return Err(StoreError::Overflow);
    }
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::subagent_control::{
        ApprovalCapabilityV1, CapabilitySnapshotV1, ChildAttemptId, ChildBindingV1, LogicalChildId,
        McpCapabilityV1, ParentBindingV1, RegisterChildV1, SandboxCapabilityV1,
    };
    use std::{fs, os::unix::fs::PermissionsExt};

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn binding() -> SubagentControlBindingV1 {
        SubagentControlBindingV1 {
            project_id: ProjectId::new("control-project").unwrap(),
            session_id: SessionId::new("control-session").unwrap(),
            root_run_id: RootRunId::new("root-run").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt").unwrap(),
            active_task_id: TaskId::new("task-1").unwrap(),
        }
    }

    fn owner(root: &tempfile::TempDir) -> SubagentControlOwner {
        SubagentControlOwner::open_or_create_owned(
            &fs::canonicalize(root.path()).unwrap(),
            binding(),
        )
        .unwrap()
    }

    fn child(binding: &SubagentControlBindingV1) -> RegisterChildV1 {
        RegisterChildV1 {
            logical_child_id: LogicalChildId::new("child-1").unwrap(),
            attempt_id: ChildAttemptId::new("attempt-1").unwrap(),
            binding: ChildBindingV1 {
                root_run_id: binding.root_run_id.clone(),
                root_attempt_id: binding.root_attempt_id.clone(),
                active_task_id: binding.active_task_id.clone(),
                parent: ParentBindingV1::Root,
            },
            role: "file-reader".into(),
            tier: "dwarf".into(),
            model: "luna".into(),
            effort: "low".into(),
            capabilities: CapabilitySnapshotV1 {
                sandbox: SandboxCapabilityV1::ReadOnly,
                write_roots: Vec::new(),
                tool_names: vec!["read".into()],
                approval: ApprovalCapabilityV1::Never,
                mcp: McpCapabilityV1::None,
            },
        }
    }

    #[test]
    fn atomically_persists_and_reloads_at_a_cas_revision() {
        let root = root();
        let mut control_owner = owner(&root);
        let initial = control_owner.snapshot().unwrap();
        assert_eq!(initial.revision, 0);
        let mut plane = initial.control_plane.clone();
        plane
            .register_before_start(child(&initial.binding))
            .unwrap();
        let saved = control_owner
            .replace(initial.revision, plane.clone())
            .unwrap();
        assert_eq!(saved.revision, 1);
        drop(control_owner);

        let restored = owner(&root).snapshot().unwrap();
        assert_eq!(restored.revision, 1);
        assert_eq!(restored.control_plane, plane);
    }

    #[test]
    fn stale_cas_fails_and_foreign_root_has_an_isolated_owner() {
        let root = root();
        let mut control_owner = owner(&root);
        let state = control_owner.snapshot().unwrap();
        let error = control_owner.replace(1, state.control_plane).unwrap_err();
        assert!(matches!(error, StoreError::CasConflict("subagent control")));
        drop(control_owner);

        let mut foreign = binding();
        foreign.root_run_id = RootRunId::new("other-root").unwrap();
        let foreign = SubagentControlOwner::open_or_create_owned(
            &fs::canonicalize(root.path()).unwrap(),
            foreign.clone(),
        )
        .unwrap()
        .snapshot()
        .unwrap();
        assert_eq!(foreign.revision, 0);
        assert_eq!(
            foreign.binding.root_run_id,
            RootRunId::new("other-root").unwrap()
        );
        assert!(foreign.control_plane.children.is_empty());
    }

    #[test]
    fn plane_from_another_root_or_task_is_rejected_before_publication() {
        let root = root();
        let mut owner = owner(&root);
        let state = owner.snapshot().unwrap();
        let mut plane = state.control_plane.clone();
        let mut request = child(&state.binding);
        request.binding.active_task_id = TaskId::new("foreign-task").unwrap();
        plane.register_before_start(request).unwrap();
        assert!(matches!(
            owner.replace(state.revision, plane),
            Err(StoreError::TargetMismatch)
        ));
        assert_eq!(owner.snapshot().unwrap().revision, 0);
    }

    #[test]
    fn second_live_owner_is_rejected_and_state_is_never_followed() {
        use std::os::unix::fs::symlink;
        let root = root();
        let first = owner(&root);
        assert!(matches!(
            SubagentControlOwner::open_or_create_owned(
                &fs::canonicalize(root.path()).unwrap(),
                binding()
            ),
            Err(StoreError::Busy)
        ));
        let state_path = first.directory.path().join(STATE_FILE);
        drop(first);
        fs::remove_file(&state_path).unwrap();
        let outside = root.path().join("outside.json");
        fs::write(&outside, b"{}").unwrap();
        symlink(&outside, &state_path).unwrap();
        assert!(
            SubagentControlOwner::open_or_create_owned(
                &fs::canonicalize(root.path()).unwrap(),
                binding()
            )
            .is_err()
        );
    }
}
