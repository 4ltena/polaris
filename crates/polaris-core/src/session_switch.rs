//! Project-owned local session switches. A started switch is never replayed
//! after an uncertain writer result; its operation ID identifies one child.

#![cfg(unix)]

use crate::{
    conversation_state::{HistoryMode, content_hash, expected_fork_snapshot},
    desktop_store::{SessionManagerStore, StoreError},
    management_effect::{
        ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
        ManagementEffectStateV1, ManagementEffectStoreError, ManagementEffectStoreV1,
        ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
        ManagementRewindDispositionV1, ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1,
        PrepareManagementEffectV1, StartedManagementEffectV1,
    },
    project_mutation::ProjectMutationLeaseV1,
    session_manager::SessionId,
    session_store::PersistedSession,
    workflow::SessionWorkflow,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchDescriptor {
    version: u8,
    operation_id: ManagementOperationIdV1,
    inventory: String,
    child_id: String,
    project: ManagementEffectHashV1,
    source_id: Option<String>,
    source_epoch: u64,
    source_generation: u64,
    source_raw_hash: Option<String>,
    initial_state_hash: Option<ManagementEffectHashV1>,
    expected_state_hash: ManagementEffectHashV1,
    expected_raw_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionSwitchError {
    #[error("Management operation record: {0}")]
    Effect(#[from] ManagementEffectStoreError),
    #[error("Conversation family storage: {0}")]
    Manager(#[from] StoreError),
    #[error("Conversation storage: {0}")]
    Session(#[from] io::Error),
    #[error("Management operation ID conflicts with an existing operation")]
    OperationCollision,
}

pub struct SessionSwitchOwner<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
    pub state_dir: &'a Path,
}

impl SessionSwitchOwner<'_> {
    /// Reconcile only this owner's unresolved effects while the project
    /// mutation lease is held. The child writer is never replayed.
    pub fn recover(
        &self,
        data_root: &Path,
        database: &Path,
        project_id: &str,
    ) -> Result<(), SessionSwitchError> {
        // An unrelated pending operation needs no session-switch writes.
        // Leave it for its owner so startup can still show recovery commands.
        let Some(pending) = self.effects.blocker()? else {
            return Ok(());
        };
        if !matches!(
            pending.request.inventory.encoded().as_str(),
            "T06" | "T16" | "F18"
        ) {
            return Ok(());
        }
        let Some(effect) = crate::sync::composite::C10CompositeBlockerV1::inspect_with_mutation(
            Path::new(project_id),
            self.state_dir,
            self.lease,
        )
        .map_err(|_| ManagementEffectStoreError::UnresolvedEffect)?
        else {
            return Ok(());
        };
        let inventory = effect.request.inventory.encoded();
        if !matches!(inventory.as_str(), "T06" | "T16" | "F18") {
            return Ok(());
        }
        let descriptor =
            read_descriptor(self.state_dir, &effect, self.effects.project_scope_hash())?;
        let child_id = SessionId::new(effect.request.operation_id.child_session_id())
            .map_err(|error| io::Error::other(error.to_string()))?;
        let root = existing_family_root(self.state_dir)?;
        let sessions_root = data_root.join("sessions-v2");
        let child_path = sessions_root.join(child_id.as_str());
        let child_exists = private_child_exists(&sessions_root, &child_path)?;
        let family_exists = root
            .as_deref()
            .map(|root| SessionManagerStore::has_child_artifacts(root, &child_id))
            .transpose()?
            .unwrap_or(false);
        let orphan_root = data_root.join("session-switch-orphans-v1");
        let orphan_path = orphan_root.join(effect.request.operation_id.as_hex());
        let orphan_exists = private_child_exists(&orphan_root, &orphan_path)?;

        match effect.state {
            ManagementEffectStateV1::Prepared
                if !child_exists && !family_exists && !orphan_exists =>
            {
                let prepared = self
                    .effects
                    .prepared_effect(effect.request.operation_id)?
                    .ok_or(ManagementEffectStoreError::StaleEffect)?;
                self.effects.deny(
                    self.lease,
                    &prepared,
                    ManagementDenialReasonV1::InvalidState,
                )?;
            }
            ManagementEffectStateV1::OutcomeUnknown { .. } if !child_exists && !family_exists => {
                let unknown = self
                    .effects
                    .unknown_effect(effect.request.operation_id)?
                    .ok_or(ManagementEffectStoreError::StaleEffect)?;
                let evidence = if orphan_exists {
                    orphan_evidence(&orphan_path, effect.request.request_hash)?
                } else {
                    ManagementEffectHashV1::digest(
                        format!(
                            "switch-child-absent-v1|{}|{}",
                            child_id.as_str(),
                            effect.request.request_hash
                        )
                        .as_bytes(),
                    )
                };
                self.effects
                    .reconcile_abandon(self.lease, &unknown, evidence)?;
            }
            ManagementEffectStateV1::OutcomeUnknown { .. } if child_exists && !orphan_exists => {
                let partial = is_partial_child(&child_path, &descriptor)?;
                if family_exists && partial {
                    return Err(io::Error::other(
                        "Conversation family conflicts with a partial fork destination",
                    )
                    .into());
                }
                if partial {
                    let orphan_root = ensure_orphan_root(data_root)?;
                    let orphan_path = orphan_root.join(effect.request.operation_id.as_hex());
                    fs::rename(&child_path, &orphan_path)?;
                    fs::File::open(&sessions_root)?.sync_all()?;
                    fs::File::open(&orphan_root)?.sync_all()?;
                    let evidence = orphan_evidence(&orphan_path, effect.request.request_hash)?;
                    let unknown = self
                        .effects
                        .unknown_effect(effect.request.operation_id)?
                        .ok_or(ManagementEffectStoreError::StaleEffect)?;
                    self.effects
                        .reconcile_abandon(self.lease, &unknown, evidence)?;
                    return Ok(());
                }
                let child =
                    PersistedSession::open(data_root, database, project_id, child_id.as_str())?;
                let snapshot = child.snapshot()?;
                if !matches_descriptor(&snapshot, &descriptor)? {
                    return Err(io::Error::other(
                        "Management operation conversation evidence mismatch",
                    )
                    .into());
                }
                fs::File::open(&child_path)?.sync_all()?;
                fs::File::open(&sessions_root)?.sync_all()?;
                SessionManagerStore::repair_published_child(
                    &family_root(self.state_dir)?,
                    &child_id,
                )?;
                let evidence = child_evidence(&snapshot);
                let unknown = self
                    .effects
                    .unknown_effect(effect.request.operation_id)?
                    .ok_or(ManagementEffectStoreError::StaleEffect)?;
                self.effects
                    .complete_unknown(self.lease, &unknown, evidence)?;
            }
            _ => {
                return Err(io::Error::other(
                    "Partial conversation evidence remains for the management operation",
                )
                .into());
            }
        }
        Ok(())
    }

    pub fn fork(
        &self,
        source: &PersistedSession,
        inventory: ManagementInventoryIdV1,
    ) -> Result<PersistedSession, SessionSwitchError> {
        if !matches!(inventory.encoded().as_str(), "T16" | "F18") {
            return Err(ManagementEffectStoreError::InvalidInventoryContract.into());
        }
        let snapshot = source.snapshot()?;
        crate::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
            Path::new(&snapshot.state.project_id),
            self.state_dir,
            self.lease,
        )
        .map_err(|_| ManagementEffectStoreError::UnresolvedEffect)?;
        let source_id = SessionId::new(snapshot.state.session_id.clone())
            .map_err(|error| io::Error::other(error.to_string()))?;
        let operation_id = new_operation_id()?;
        let child_id = SessionId::new(operation_id.child_session_id())
            .map_err(|error| io::Error::other(error.to_string()))?;
        let expected = expected_fork_snapshot(&snapshot, child_id.as_str(), "")?;
        let mut initial = expected.state.clone();
        initial.generation = 0;
        initial.raw_commit_offset = 0;
        initial.raw_commit_hash = content_hash(&[]);
        initial.recent_turn_ids.clear();
        let descriptor = SwitchDescriptor {
            version: 1,
            operation_id,
            inventory: inventory.encoded(),
            child_id: child_id.as_str().into(),
            project: self.effects.project_scope_hash(),
            source_id: Some(source_id.as_str().into()),
            source_epoch: snapshot.state.epoch,
            source_generation: snapshot.state.generation,
            source_raw_hash: Some(snapshot.raw_hash.clone()),
            initial_state_hash: Some(state_hash(&initial)?),
            expected_state_hash: state_hash(&expected.state)?,
            expected_raw_hash: expected.raw_hash,
        };
        let descriptor_bytes = write_descriptor(self.state_dir, &descriptor)?;
        let request = self.request(
            operation_id,
            inventory,
            Some(source_id.as_str()),
            snapshot.state.epoch,
            snapshot.state.generation,
            &descriptor_bytes,
        );
        let prepared = self.prepare(request)?;
        let root = match family_root(self.state_dir) {
            Ok(root) => root,
            Err(error) => {
                return Err(self.finish_error(&prepared, None, StoreError::Io(error)));
            }
        };
        let mut started = None;
        let result = SessionManagerStore::fork_family_owned(
            &root,
            &source_id,
            &child_id,
            |target, _state| {
                let handle = self
                    .effects
                    .start(self.lease, &prepared)
                    .map_err(effect_as_store_io)?;
                started = Some(handle);
                source
                    .fork_with_id(target.as_str(), "")
                    .map_err(StoreError::Io)
            },
        );
        let child = match result {
            Ok(child) => child,
            Err(error) => return Err(self.finish_error(&prepared, started.as_ref(), error)),
        };
        self.complete(
            started.as_ref().expect("fork publish started"),
            &child,
            &descriptor,
        )?;
        Ok(child)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_new(
        &self,
        source: Option<&PersistedSession>,
        data_root: &Path,
        database: &Path,
        project_id: &str,
        mode: HistoryMode,
        workflow: Option<&SessionWorkflow>,
        inventory: ManagementInventoryIdV1,
    ) -> Result<PersistedSession, SessionSwitchError> {
        if inventory.encoded() != "T06" {
            return Err(ManagementEffectStoreError::InvalidInventoryContract.into());
        }
        let source_snapshot = source.map(PersistedSession::snapshot).transpose()?;
        if source_snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.state.project_id != project_id)
        {
            return Err(
                io::Error::other("New conversation project does not match the source").into(),
            );
        }
        crate::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
            Path::new(project_id),
            self.state_dir,
            self.lease,
        )
        .map_err(|_| ManagementEffectStoreError::UnresolvedEffect)?;
        let operation_id = new_operation_id()?;
        let child_id = SessionId::new(operation_id.child_session_id())
            .map_err(|error| io::Error::other(error.to_string()))?;
        let expected = PersistedSession::new_state(project_id, child_id.as_str(), mode, workflow)?;
        let descriptor = SwitchDescriptor {
            version: 1,
            operation_id,
            inventory: inventory.encoded(),
            child_id: child_id.as_str().into(),
            project: self.effects.project_scope_hash(),
            source_id: source_snapshot
                .as_ref()
                .map(|snapshot| snapshot.state.session_id.clone()),
            source_epoch: source_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.state.epoch),
            source_generation: source_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.state.generation),
            source_raw_hash: source_snapshot
                .as_ref()
                .map(|snapshot| snapshot.raw_hash.clone()),
            initial_state_hash: None,
            expected_state_hash: state_hash(&expected)?,
            expected_raw_hash: content_hash(&[]),
        };
        let descriptor_bytes = write_descriptor(self.state_dir, &descriptor)?;
        let request = self.request(
            operation_id,
            inventory,
            source_snapshot
                .as_ref()
                .map(|snapshot| snapshot.state.session_id.as_str()),
            source_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.state.epoch),
            source_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.state.generation),
            &descriptor_bytes,
        );
        let prepared = self.prepare(request)?;
        let root = match family_root(self.state_dir) {
            Ok(root) => root,
            Err(error) => {
                return Err(self.finish_error(&prepared, None, StoreError::Io(error)));
            }
        };
        let mut started = None;
        let result = SessionManagerStore::create_family_owned(&root, &child_id, |target| {
            let handle = self
                .effects
                .start(self.lease, &prepared)
                .map_err(effect_as_store_io)?;
            started = Some(handle);
            PersistedSession::create_with_id(
                data_root,
                database,
                project_id,
                target.as_str(),
                mode,
                workflow,
            )
            .map_err(StoreError::Io)
        });
        let child = match result {
            Ok(child) => child,
            Err(error) => return Err(self.finish_error(&prepared, started.as_ref(), error)),
        };
        self.complete(
            started.as_ref().expect("new publish started"),
            &child,
            &descriptor,
        )?;
        Ok(child)
    }

    fn request(
        &self,
        operation_id: ManagementOperationIdV1,
        inventory: ManagementInventoryIdV1,
        source: Option<&str>,
        generation: u64,
        revision: u64,
        request_bytes: &[u8],
    ) -> PrepareManagementEffectV1 {
        PrepareManagementEffectV1 {
            operation_id,
            inventory,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: source.map(|id| ManagementEffectHashV1::digest(id.as_bytes())),
                generation,
                revision,
                permission: ManagementPermissionScopeV1::LocalSession,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(request_bytes),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        }
    }

    fn prepare(
        &self,
        request: PrepareManagementEffectV1,
    ) -> Result<crate::management_effect::PreparedManagementEffectV1, SessionSwitchError> {
        match self
            .effects
            .prepare(self.lease, self.effects.cursor()?, request)?
        {
            PrepareManagementEffectOutcomeV1::Prepared(effect) => Ok(effect),
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                Err(SessionSwitchError::OperationCollision)
            }
        }
    }

    fn finish_error(
        &self,
        prepared: &crate::management_effect::PreparedManagementEffectV1,
        started: Option<&StartedManagementEffectV1>,
        error: StoreError,
    ) -> SessionSwitchError {
        let recorded = if let Some(started) = started {
            self.effects
                .mark_outcome_unknown(
                    self.lease,
                    started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                )
                .map(|_| ())
        } else {
            self.effects
                .deny(self.lease, prepared, ManagementDenialReasonV1::InvalidState)
                .map(|_| ())
        };
        match recorded {
            Ok(()) => SessionSwitchError::Manager(error),
            Err(record_error) => SessionSwitchError::Effect(record_error),
        }
    }

    fn complete(
        &self,
        started: &StartedManagementEffectV1,
        child: &PersistedSession,
        descriptor: &SwitchDescriptor,
    ) -> Result<(), SessionSwitchError> {
        let snapshot = match child.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.effects.mark_outcome_unknown(
                    self.lease,
                    started,
                    ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                )?;
                return Err(error.into());
            }
        };
        if !matches_descriptor(&snapshot, descriptor)? {
            self.effects.mark_outcome_unknown(
                self.lease,
                started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            )?;
            return Err(io::Error::other(
                "Fork destination storage evidence does not match the pre-operation record",
            )
            .into());
        }
        let evidence = child_evidence(&snapshot);
        match self.effects.complete_started(self.lease, started, evidence) {
            Ok(_) => Ok(()),
            Err(error) => {
                let _ = self.effects.mark_outcome_unknown(
                    self.lease,
                    started,
                    ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                );
                Err(error.into())
            }
        }
    }
}

fn state_hash(
    state: &crate::conversation_state::ConversationStateV2,
) -> io::Result<ManagementEffectHashV1> {
    Ok(ManagementEffectHashV1::digest(&serde_json::to_vec(state)?))
}

fn matches_descriptor(
    snapshot: &crate::conversation_state::ConversationSnapshot,
    descriptor: &SwitchDescriptor,
) -> io::Result<bool> {
    Ok(snapshot.state.session_id == descriptor.child_id
        && snapshot.raw_hash == descriptor.expected_raw_hash
        && state_hash(&snapshot.state)? == descriptor.expected_state_hash)
}

fn switch_descriptor_root(state_dir: &Path) -> io::Result<std::path::PathBuf> {
    let parent = fs::canonicalize(state_dir)?;
    let root = parent.join("session-switches-v1");
    match fs::symlink_metadata(&root) {
        Ok(metadata) if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::other(
                "Conversation switch record storage is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&root)?;
            fs::File::open(&parent)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    fs::canonicalize(root)
}

fn descriptor_path(root: &Path, id: ManagementOperationIdV1) -> std::path::PathBuf {
    root.join(format!("{}.json", id.as_hex()))
}

fn write_descriptor(state_dir: &Path, descriptor: &SwitchDescriptor) -> io::Result<Vec<u8>> {
    let root = switch_descriptor_root(state_dir)?;
    let bytes = serde_json::to_vec(descriptor)?;
    if bytes.len() > 16 * 1024 {
        return Err(io::Error::other(
            "Conversation switch record exceeds the limit",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(descriptor_path(&root, descriptor.operation_id))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::File::open(root)?.sync_all()?;
    Ok(bytes)
}

fn read_descriptor(
    state_dir: &Path,
    effect: &crate::management_effect::ManagementEffectSnapshotV1,
    project: ManagementEffectHashV1,
) -> io::Result<SwitchDescriptor> {
    let parent = fs::canonicalize(state_dir)?;
    let root = parent.join("session-switches-v1");
    let root_metadata = fs::symlink_metadata(&root)?;
    if !root_metadata.file_type().is_dir() || root_metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "Conversation switch record storage is not a directory",
        ));
    }
    let path = descriptor_path(&root, effect.request.operation_id);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > 16 * 1024
    {
        return Err(io::Error::other(
            "Invalid conversation switch record format",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    let descriptor: SwitchDescriptor = serde_json::from_slice(&bytes)?;
    if bytes.len() > 16 * 1024
        || serde_json::to_vec(&descriptor)? != bytes
        || effect.request.request_hash != ManagementEffectHashV1::digest(&bytes)
        || descriptor.version != 1
        || descriptor.operation_id != effect.request.operation_id
        || descriptor.inventory != effect.request.inventory.encoded()
        || descriptor.child_id != effect.request.operation_id.child_session_id()
        || descriptor.project != project
        || descriptor.project != effect.request.scope.project
        || descriptor
            .source_id
            .as_deref()
            .map(|source| ManagementEffectHashV1::digest(source.as_bytes()))
            != effect.request.scope.session
        || descriptor.source_epoch != effect.request.scope.generation
        || descriptor.source_generation != effect.request.scope.revision
        || matches!(descriptor.inventory.as_str(), "T16" | "F18")
            != descriptor.initial_state_hash.is_some()
    {
        return Err(io::Error::other(
            "Conversation switch record and management operation mismatch",
        ));
    }
    Ok(descriptor)
}

fn is_partial_child(child_path: &Path, descriptor: &SwitchDescriptor) -> io::Result<bool> {
    let state_path = child_path.join("state.json");
    let metadata = match fs::symlink_metadata(&state_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "Fork destination marker is not a regular file",
        ));
    }
    let state: crate::conversation_state::ConversationStateV2 =
        serde_json::from_slice(&fs::read(state_path)?)?;
    if state.generation == 0
        && let Some(expected) = descriptor.initial_state_hash
    {
        return Ok(state_hash(&state)? == expected);
    }
    Ok(false)
}

fn ensure_orphan_root(data_root: &Path) -> io::Result<std::path::PathBuf> {
    let parent = fs::canonicalize(data_root)?;
    let root = parent.join("session-switch-orphans-v1");
    match fs::symlink_metadata(&root) {
        Ok(metadata) if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::other(
                "Fork destination archive is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&root)?;
            fs::File::open(&parent)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    fs::canonicalize(root)
}

fn orphan_evidence(
    orphan: &Path,
    request_hash: ManagementEffectHashV1,
) -> io::Result<ManagementEffectHashV1> {
    let mut hasher = Sha256::new();
    let mut entries = 0_usize;
    hash_orphan_tree(orphan, &mut hasher, &mut entries, 0)?;
    let tree_hash = format!("{:x}", hasher.finalize());
    Ok(ManagementEffectHashV1::digest(
        format!("switch-orphan-v1|{request_hash}|{tree_hash}").as_bytes(),
    ))
}

fn hash_orphan_tree(
    directory: &Path,
    hasher: &mut Sha256,
    entries: &mut usize,
    depth: usize,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if depth > 8 {
        return Err(io::Error::other(
            "Archived conversation depth exceeds the limit",
        ));
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("Invalid archived conversation directory"));
    }
    let mut children = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        *entries += 1;
        if *entries > 4096 {
            return Err(io::Error::other(
                "Archived conversation file count exceeds the limit",
            ));
        }
        let name = child.file_name();
        let bytes = name.as_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other("Archived conversation contains a symlink"));
        }
        if metadata.file_type().is_dir() {
            hasher.update(b"directory");
            hash_orphan_tree(&path, hasher, entries, depth + 1)?;
        } else if metadata.file_type().is_file() {
            hasher.update(b"file");
            hasher.update(metadata.len().to_le_bytes());
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            file.sync_all()?;
        } else {
            return Err(io::Error::other(
                "Archived conversation contains a special file",
            ));
        }
    }
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn child_evidence(
    snapshot: &crate::conversation_state::ConversationSnapshot,
) -> ManagementEffectHashV1 {
    ManagementEffectHashV1::digest(
        format!(
            "{}|{}|{}",
            snapshot.state.session_id, snapshot.state.generation, snapshot.raw_hash
        )
        .as_bytes(),
    )
}

fn private_child_exists(parent: &Path, child: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(parent) {
        Ok(metadata) if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::other("Conversation storage is not a directory"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    match fs::symlink_metadata(child) {
        Ok(metadata) if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {
            Err(io::Error::other("Fork destination is not a directory"))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn new_operation_id() -> Result<ManagementOperationIdV1, SessionSwitchError> {
    ManagementOperationIdV1::random().map_err(|error| io::Error::other(error.to_string()).into())
}

fn family_root(state_dir: &Path) -> io::Result<std::path::PathBuf> {
    let parent = fs::canonicalize(state_dir)?;
    let root = parent.join("tui-admissions-v1");
    match fs::symlink_metadata(&root) {
        Ok(metadata) if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() => {
            return Err(io::Error::other(
                "Conversation family storage is not a directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&root)?;
            fs::File::open(&parent)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let canonical = fs::canonicalize(root)?;
    if canonical.parent() != Some(parent.as_path()) {
        return Err(io::Error::other(
            "Conversation family storage is outside the state area",
        ));
    }
    Ok(canonical)
}

fn existing_family_root(state_dir: &Path) -> io::Result<Option<std::path::PathBuf>> {
    let parent = fs::canonicalize(state_dir)?;
    let root = parent.join("tui-admissions-v1");
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(fs::canonicalize(root)?))
        }
        Ok(_) => Err(io::Error::other(
            "Conversation family storage is not a directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn effect_as_store_io(error: ManagementEffectStoreError) -> StoreError {
    StoreError::Io(io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_mutation::ProjectMutationOwnerV1;
    use std::path::PathBuf;

    struct Fixture {
        _temp: tempfile::TempDir,
        project: PathBuf,
        state: PathBuf,
        data: PathBuf,
        lease: ProjectMutationLeaseV1,
        effects: ManagementEffectStoreV1,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let project = temp.path().join("project");
            let state = temp.path().join("state");
            let data = temp.path().join("data");
            fs::create_dir(&project).unwrap();
            fs::create_dir(project.join(".polaris")).unwrap();
            fs::create_dir(&state).unwrap();
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
            fs::create_dir(&data).unwrap();
            crate::rewind_workspace::WorkspaceOwner::open(
                &project,
                &state.join("rewind-workspace"),
                Default::default(),
            )
            .unwrap();
            let lease = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace"))
                .unwrap()
                .try_mutation()
                .unwrap();
            let effects =
                ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
            Self {
                _temp: temp,
                project,
                state,
                data,
                lease,
                effects,
            }
        }

        fn owner(&self) -> SessionSwitchOwner<'_> {
            SessionSwitchOwner {
                effects: &self.effects,
                lease: &self.lease,
                state_dir: &self.state,
            }
        }

        fn reopen(self) -> Self {
            let Self {
                _temp,
                project,
                state,
                data,
                lease,
                effects,
            } = self;
            drop(effects);
            drop(lease);
            let lease = ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace"))
                .unwrap()
                .try_mutation()
                .unwrap();
            let effects =
                ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
            Self {
                _temp,
                project,
                state,
                data,
                lease,
                effects,
            }
        }

        fn prepare_new(&self) -> (ManagementOperationIdV1, String) {
            let owner = self.owner();
            let id = new_operation_id().unwrap();
            let child = id.child_session_id();
            let inventory = ManagementInventoryIdV1::parse("T06").unwrap();
            let state = PersistedSession::new_state(
                self.project.to_str().unwrap(),
                &child,
                HistoryMode::Legacy,
                None,
            )
            .unwrap();
            let descriptor = SwitchDescriptor {
                version: 1,
                operation_id: id,
                inventory: inventory.encoded(),
                child_id: child.clone(),
                project: self.effects.project_scope_hash(),
                source_id: None,
                source_epoch: 0,
                source_generation: 0,
                source_raw_hash: None,
                initial_state_hash: None,
                expected_state_hash: state_hash(&state).unwrap(),
                expected_raw_hash: content_hash(&[]),
            };
            let bytes = write_descriptor(&self.state, &descriptor).unwrap();
            let request = owner.request(id, inventory, None, 0, 0, &bytes);
            owner.prepare(request).unwrap();
            (id, child)
        }

        fn mark_unknown(&self, id: ManagementOperationIdV1) {
            let prepared = self.effects.prepared_effect(id).unwrap().unwrap();
            let started = self.effects.start(&self.lease, &prepared).unwrap();
            self.effects
                .mark_outcome_unknown(
                    &self.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                )
                .unwrap();
        }

        fn prepare_fork(
            &self,
            source: &PersistedSession,
            inventory_name: &str,
        ) -> (
            ManagementOperationIdV1,
            String,
            crate::conversation_state::ConversationStateV2,
        ) {
            let owner = self.owner();
            let source_snapshot = source.snapshot().unwrap();
            let id = new_operation_id().unwrap();
            let child = id.child_session_id();
            let inventory = ManagementInventoryIdV1::parse(inventory_name).unwrap();
            let expected = expected_fork_snapshot(&source_snapshot, &child, "").unwrap();
            let mut initial = expected.state.clone();
            initial.generation = 0;
            initial.raw_commit_offset = 0;
            initial.raw_commit_hash = content_hash(&[]);
            initial.recent_turn_ids.clear();
            let descriptor = SwitchDescriptor {
                version: 1,
                operation_id: id,
                inventory: inventory.encoded(),
                child_id: child.clone(),
                project: self.effects.project_scope_hash(),
                source_id: Some(source_snapshot.state.session_id.clone()),
                source_epoch: source_snapshot.state.epoch,
                source_generation: source_snapshot.state.generation,
                source_raw_hash: Some(source_snapshot.raw_hash),
                initial_state_hash: Some(state_hash(&initial).unwrap()),
                expected_state_hash: state_hash(&expected.state).unwrap(),
                expected_raw_hash: expected.raw_hash,
            };
            let bytes = write_descriptor(&self.state, &descriptor).unwrap();
            let request = owner.request(
                id,
                inventory,
                Some(&source_snapshot.state.session_id),
                source_snapshot.state.epoch,
                source_snapshot.state.generation,
                &bytes,
            );
            owner.prepare(request).unwrap();
            (id, child, initial)
        }
    }

    #[test]
    fn switch_owner_rejects_wrong_inventory_before_writing_a_descriptor() {
        let fixture = Fixture::new();
        let cursor = fixture.effects.cursor().unwrap();
        let descriptors = fixture.state.join("session-switches-v1");
        assert!(matches!(
            fixture.owner().create_new(
                None,
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
                HistoryMode::Legacy,
                None,
                ManagementInventoryIdV1::parse("T16").unwrap(),
            ),
            Err(SessionSwitchError::Effect(
                ManagementEffectStoreError::InvalidInventoryContract
            ))
        ));
        assert!(!descriptors.exists());
        assert_eq!(fixture.effects.cursor().unwrap(), cursor);

        let source_id = new_operation_id().unwrap().child_session_id();
        let source = PersistedSession::create_with_id(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            &source_id,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        assert!(matches!(
            fixture
                .owner()
                .fork(&source, ManagementInventoryIdV1::parse("T06").unwrap()),
            Err(SessionSwitchError::Effect(
                ManagementEffectStoreError::InvalidInventoryContract
            ))
        ));
        assert!(!descriptors.exists());
        assert_eq!(fixture.effects.cursor().unwrap(), cursor);
    }

    #[test]
    fn restart_denies_prepared_switch_without_a_child() {
        let fixture = Fixture::new();
        let (id, _) = fixture.prepare_new();
        let fixture = fixture.reopen();
        fixture
            .owner()
            .recover(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            fixture.effects.effect(id).unwrap().unwrap().state,
            ManagementEffectStateV1::Denied { .. }
        ));
    }

    #[test]
    fn restart_quarantines_unpublished_child_and_preserves_evidence() {
        let fixture = Fixture::new();
        let (id, child) = fixture.prepare_new();
        fixture.mark_unknown(id);
        let child_path = fixture.data.join("sessions-v2").join(&child);
        fs::create_dir_all(&child_path).unwrap();
        fs::set_permissions(&child_path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(child_path.join("raw.jsonl"), b"partial raw").unwrap();
        let fixture = fixture.reopen();
        fixture
            .owner()
            .recover(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
            )
            .unwrap();
        assert!(!child_path.exists());
        assert_eq!(
            fs::read(
                fixture
                    .data
                    .join("session-switch-orphans-v1")
                    .join(id.as_hex())
                    .join("raw.jsonl")
            )
            .unwrap(),
            b"partial raw"
        );
        assert!(matches!(
            fixture.effects.effect(id).unwrap().unwrap().state,
            ManagementEffectStateV1::ReconciledAbandon { .. }
        ));
    }

    #[test]
    fn restart_repairs_complete_child_and_rejects_changed_marker() {
        let fixture = Fixture::new();
        let (id, child_id) = fixture.prepare_new();
        fixture.mark_unknown(id);
        PersistedSession::create_with_id(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            &child_id,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let fixture = fixture.reopen();
        fixture
            .owner()
            .recover(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            fixture.effects.effect(id).unwrap().unwrap().state,
            ManagementEffectStateV1::Completed { .. }
        ));
        let family = family_root(&fixture.state).unwrap();
        let (_, manager) =
            SessionManagerStore::open_family_owned(&family, &SessionId::new(child_id).unwrap())
                .unwrap();
        assert_eq!(manager.sessions.len(), 1);

        let (changed_id, changed_child) = fixture.prepare_new();
        fixture.mark_unknown(changed_id);
        let changed = PersistedSession::create_with_id(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            &changed_child,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let mut changed_state = changed.snapshot().unwrap().state;
        changed_state.generation = 1;
        fs::write(
            fixture
                .data
                .join("sessions-v2")
                .join(&changed_child)
                .join("state.json"),
            serde_json::to_vec(&changed_state).unwrap(),
        )
        .unwrap();
        let fixture = fixture.reopen();
        assert!(
            fixture
                .owner()
                .recover(
                    &fixture.data,
                    &fixture.state.join("memory.sqlite3"),
                    fixture.project.to_str().unwrap()
                )
                .is_err()
        );
        assert!(matches!(
            fixture.effects.effect(changed_id).unwrap().unwrap().state,
            ManagementEffectStateV1::OutcomeUnknown { .. }
        ));
    }

    #[test]
    fn reopening_started_switch_classifies_unknown_before_reconciliation() {
        let fixture = Fixture::new();
        let (id, _) = fixture.prepare_new();
        let prepared = fixture.effects.prepared_effect(id).unwrap().unwrap();
        fixture.effects.start(&fixture.lease, &prepared).unwrap();
        let fixture = fixture.reopen();
        assert!(matches!(
            fixture.effects.effect(id).unwrap().unwrap().state,
            ManagementEffectStateV1::OutcomeUnknown { .. }
        ));
        fixture
            .owner()
            .recover(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            fixture.effects.effect(id).unwrap().unwrap().state,
            ManagementEffectStateV1::ReconciledAbandon { .. }
        ));
    }

    #[test]
    fn fork_initial_marker_and_raw_tail_are_quarantined_after_reopen() {
        for inventory in ["T16", "F18"] {
            let fixture = Fixture::new();
            let source = PersistedSession::create_with_id(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
                "11111111-1111-4111-8111-111111111111",
                HistoryMode::Legacy,
                None,
            )
            .unwrap();
            let (id, child, initial) = fixture.prepare_fork(&source, inventory);
            fixture.mark_unknown(id);
            PersistedSession::create_with_id(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
                &child,
                HistoryMode::Legacy,
                None,
            )
            .unwrap();
            let child_path = fixture.data.join("sessions-v2").join(&child);
            fs::write(
                child_path.join("state.json"),
                serde_json::to_vec(&initial).unwrap(),
            )
            .unwrap();
            fs::write(child_path.join("raw.jsonl"), b"unpublished tail").unwrap();
            let fixture = fixture.reopen();
            fixture
                .owner()
                .recover(
                    &fixture.data,
                    &fixture.state.join("memory.sqlite3"),
                    fixture.project.to_str().unwrap(),
                )
                .unwrap();
            assert!(matches!(
                fixture.effects.effect(id).unwrap().unwrap().state,
                ManagementEffectStateV1::ReconciledAbandon { .. }
            ));
            assert_eq!(
                fs::read(
                    fixture
                        .data
                        .join("session-switch-orphans-v1")
                        .join(id.as_hex())
                        .join("raw.jsonl")
                )
                .unwrap(),
                b"unpublished tail"
            );
            assert_eq!(source.snapshot().unwrap().state.generation, 0);
        }
    }

    #[test]
    fn complete_fork_descriptor_repairs_family_after_reopen() {
        for inventory in ["T16", "F18"] {
            let fixture = Fixture::new();
            let source = PersistedSession::create_with_id(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
                "11111111-1111-4111-8111-111111111111",
                HistoryMode::Legacy,
                None,
            )
            .unwrap();
            let (id, child, _) = fixture.prepare_fork(&source, inventory);
            fixture.mark_unknown(id);
            source.fork_with_id(&child, "").unwrap();
            let fixture = fixture.reopen();
            fixture
                .owner()
                .recover(
                    &fixture.data,
                    &fixture.state.join("memory.sqlite3"),
                    fixture.project.to_str().unwrap(),
                )
                .unwrap();
            assert!(matches!(
                fixture.effects.effect(id).unwrap().unwrap().state,
                ManagementEffectStateV1::Completed { .. }
            ));
            let (_, family) = SessionManagerStore::open_family_owned(
                &family_root(&fixture.state).unwrap(),
                &SessionId::new(child).unwrap(),
            )
            .unwrap();
            assert_eq!(family.sessions.len(), 1);
        }
    }

    #[test]
    fn orphan_rename_before_receipt_is_reconciled_after_reopen() {
        let fixture = Fixture::new();
        let (id, child) = fixture.prepare_new();
        fixture.mark_unknown(id);
        let sessions_root = fixture.data.join("sessions-v2");
        let child_path = sessions_root.join(child);
        fs::create_dir_all(&child_path).unwrap();
        fs::set_permissions(&child_path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(child_path.join("raw.jsonl"), b"kept").unwrap();
        let orphan_root = ensure_orphan_root(&fixture.data).unwrap();
        fs::rename(child_path, orphan_root.join(id.as_hex())).unwrap();
        fs::File::open(&sessions_root).unwrap().sync_all().unwrap();
        fs::File::open(&orphan_root).unwrap().sync_all().unwrap();
        let fixture = fixture.reopen();
        fixture
            .owner()
            .recover(
                &fixture.data,
                &fixture.state.join("memory.sqlite3"),
                fixture.project.to_str().unwrap(),
            )
            .unwrap();
        assert!(matches!(
            fixture.effects.effect(id).unwrap().unwrap().state,
            ManagementEffectStateV1::ReconciledAbandon { .. }
        ));
        assert_eq!(
            fs::read(orphan_root.join(id.as_hex()).join("raw.jsonl")).unwrap(),
            b"kept"
        );
    }
}
