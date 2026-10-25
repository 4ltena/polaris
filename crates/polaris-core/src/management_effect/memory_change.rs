//! One project owner for explicit import, embedding, indexing and session forgetting.
//! Bodies stay in the memory transaction; management effects persist hashes only.

use super::{
    ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
    ManagementEffectStateV1, ManagementEffectStoreV1, ManagementInventoryIdV1,
    ManagementOperationIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
    ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1, PrepareManagementEffectV1,
    PreparedManagementEffectV1,
};
use crate::{
    conversation_state::ConversationStore,
    project_mutation::{ProjectMutationLeaseV1, directory_names_bounded, open_directory_nofollow},
};
use polaris_auth::protection::{ProtectedIdentity, ProtectedPathsSnapshot};
use polaris_memory::{
    MemoryStore,
    management::{
        MemoryChangeKindV1, MemoryChangeReceiptV1, MemoryChangeRequestV1, validate_session_id,
    },
};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

pub struct MemoryPreparedChangeV1 {
    request: MemoryChangeRequestV1,
    prepared: Option<PreparedManagementEffectV1>,
}
impl MemoryPreparedChangeV1 {
    pub fn request(&self) -> &MemoryChangeRequestV1 {
        &self.request
    }
}

pub struct MemoryChangeOwnerV1<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
    pub project_root: &'a Path,
    pub data_root: &'a Path,
    pub active_session: Option<&'a str>,
}

fn failure(message: &str) -> String {
    message.to_owned()
}
fn protected_snapshot() -> Result<ProtectedPathsSnapshot, String> {
    polaris_auth::protection::with_protected_paths(Clone::clone)
        .map_err(|_| failure("Cannot verify authentication store protection metadata"))
}
fn protected(path: &Path, metadata: &fs::Metadata, snapshot: &ProtectedPathsSnapshot) -> bool {
    snapshot
        .paths()
        .iter()
        .any(|registered| registered == path || registered.starts_with(path))
        || snapshot.identities().contains(&ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
}

impl MemoryChangeOwnerV1<'_> {
    fn project_identity(&self) -> Result<String, String> {
        let project = fs::canonicalize(self.project_root).map_err(|e| e.to_string())?;
        let identity = project
            .to_str()
            .ok_or_else(|| failure("Project name is not UTF-8"))?;
        if !project.is_dir()
            || ManagementEffectHashV1::digest(identity.as_bytes())
                != self.effects.project_scope_hash()
            || self.lease.store_root() != self.effects.state_dir().join("rewind-workspace")
        {
            return Err(failure("Memory owner project mismatch"));
        }
        let data = fs::canonicalize(self.data_root).map_err(|e| e.to_string())?;
        if data
            .join("state")
            .join(crate::project::project_id(&project))
            != self.effects.state_dir()
        {
            return Err(failure("Memory storage does not match the target project"));
        }
        Ok(identity.to_owned())
    }

    /// Initialize only the host-selected database after nofollow/protection checks.
    pub fn open_store(&self) -> Result<MemoryStore, String> {
        self.project_identity()?;
        let snapshot = protected_snapshot()?;
        let directory =
            open_directory_nofollow(self.effects.state_dir()).map_err(|e| e.to_string())?;
        let meta = directory.metadata().map_err(|e| e.to_string())?;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o22 != 0
            || protected(self.effects.state_dir(), &meta, &snapshot)
        {
            return Err(failure("Memory storage is protected"));
        }
        let path = self.effects.state_dir().join("memory.sqlite3");
        if snapshot.paths().contains(&path) {
            return Err(failure("Memory storage is protected"));
        }
        let name = c"memory.sqlite3";
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file()
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::geteuid() }
            || protected(&path, &meta, &snapshot)
        {
            return Err(failure("Memory DB is not a regular dedicated file"));
        }
        named_identity(&directory, name.to_bytes(), &meta).map_err(|e| e.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        let store = MemoryStore::open(&path).map_err(|e| e.to_string())?;
        self.bind(&store)?;
        named_identity(&directory, name.to_bytes(), &meta).map_err(|e| e.to_string())?;
        Ok(store)
    }

    fn bind(&self, store: &MemoryStore) -> Result<String, String> {
        let project = self.project_identity()?;
        let path = Path::new(
            store
                .database_path()
                .ok_or_else(|| failure("A persistent memory DB is required"))?,
        );
        if path != self.effects.state_dir().join("memory.sqlite3") {
            return Err(failure("Memory DB does not match the owner"));
        }
        let parent =
            open_directory_nofollow(self.effects.state_dir()).map_err(|e| e.to_string())?;
        let file = open_child(&parent, b"memory.sqlite3", false).map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7177 != 0
            || meta.nlink() != 1
            || protected(path, &meta, &protected_snapshot()?)
        {
            return Err(failure("Memory DB protection state changed"));
        }
        Ok(project)
    }

    pub fn prepare(
        &self,
        store: &MemoryStore,
        request: MemoryChangeRequestV1,
    ) -> Result<MemoryPreparedChangeV1, String> {
        let project = self.bind(store)?;
        if request
            .change
            .session_id()
            .is_some_and(|s| self.active_session == Some(s))
        {
            return Err(failure("Cannot delete the active session"));
        }
        let hash = store
            .validate_change(&project, &request)
            .map_err(|e| e.to_string())?;
        let operation_id = request
            .operation_id
            .parse()
            .map_err(|_| failure("Invalid operation ID"))?;
        let effect = PrepareManagementEffectV1 {
            operation_id,
            inventory: ManagementInventoryIdV1::parse(request.change.kind().inventory())
                .map_err(|e| e.to_string())?,
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: request
                    .change
                    .session_id()
                    .map(|s| ManagementEffectHashV1::digest(s.as_bytes())),
                generation: 1,
                revision: request.expected_revision,
                permission: if request.change.kind() == MemoryChangeKindV1::Forget {
                    ManagementPermissionScopeV1::LocalSession
                } else {
                    ManagementPermissionScopeV1::Project
                },
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::from_hex(&hash).map_err(|e| e.to_string())?,
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let prepared = match self
            .effects
            .prepare(
                self.lease,
                self.effects.cursor().map_err(|e| e.to_string())?,
                effect,
            )
            .map_err(|e| e.to_string())?
        {
            PrepareManagementEffectOutcomeV1::Prepared(prepared) => Some(prepared),
            PrepareManagementEffectOutcomeV1::Existing(_) => None,
        };
        Ok(MemoryPreparedChangeV1 { request, prepared })
    }

    pub fn apply(
        &self,
        store: &mut MemoryStore,
        change: &MemoryPreparedChangeV1,
        accepted: bool,
    ) -> Result<Option<MemoryChangeReceiptV1>, String> {
        let project = self.bind(store)?;
        let operation_id = change
            .request
            .operation_id
            .parse()
            .map_err(|_| failure("Invalid operation ID"))?;
        let Some(prepared) = &change.prepared else {
            if self
                .effects
                .effect(operation_id)
                .map_err(|e| e.to_string())?
                .is_some_and(|effect| {
                    matches!(effect.state, ManagementEffectStateV1::Completed { .. })
                })
            {
                return self.reconcile(store, operation_id).map(Some);
            }
            return Err(failure(
                "The existing operation outcome is unresolved. Explicit reconciliation is required",
            ));
        };
        if !accepted {
            self.effects
                .deny(self.lease, prepared, ManagementDenialReasonV1::UserDeclined)
                .map_err(|e| e.to_string())?;
            return Ok(None);
        }
        if let Err(error) = store.validate_change(&project, &change.request) {
            self.effects
                .deny(self.lease, prepared, ManagementDenialReasonV1::StaleRequest)
                .map_err(|e| e.to_string())?;
            return Err(error.to_string());
        }
        if change
            .request
            .change
            .session_id()
            .is_some_and(|s| self.active_session == Some(s))
        {
            self.effects
                .deny(self.lease, prepared, ManagementDenialReasonV1::InvalidState)
                .map_err(|e| e.to_string())?;
            return Err(failure("Cannot delete the active session"));
        }
        let _memory_lock = if change.request.change.session_id().is_some() {
            let directory =
                open_directory_nofollow(self.effects.state_dir()).map_err(|e| e.to_string())?;
            let name = c"memory.lock";
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let meta = file.metadata().map_err(|e| e.to_string())?;
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o7177 != 0
                || protected(
                    &self.effects.state_dir().join("memory.lock"),
                    &meta,
                    &protected_snapshot()?,
                )
            {
                return Err(failure("Memory deletion lock is protected"));
            }
            file.try_lock().map_err(|e| e.to_string())?;
            Some(file)
        } else {
            None
        };
        // Preflight reads no bodies until protection and the complete pinned tree
        // have passed. Keep the raw writer lock until all removals are finished.
        let removal = if let Some(session) = change.request.change.session_id() {
            match self.prepare_removal(&project, session) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    self.effects
                        .deny(
                            self.lease,
                            prepared,
                            ManagementDenialReasonV1::PermissionDenied,
                        )
                        .map_err(|e| e.to_string())?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let raw_store = if let Some(session) = change.request.change.session_id() {
            let raw_path = self.data_root.join("sessions-v2").join(session);
            if root_present(&raw_path)? {
                match ConversationStore::open(self.data_root, &project, session) {
                    Ok(store) => Some(store),
                    Err(error) => {
                        self.effects
                            .deny(self.lease, prepared, ManagementDenialReasonV1::InvalidState)
                            .map_err(|e| e.to_string())?;
                        return Err(error.to_string());
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let raw_lock = raw_store
            .as_ref()
            .map(ConversationStore::lock)
            .transpose()
            .map_err(|e| {
                let _ =
                    self.effects
                        .deny(self.lease, prepared, ManagementDenialReasonV1::InvalidState);
                e.to_string()
            })?;
        if let Some(lock) = &raw_lock {
            let snapshot = lock.snapshot().map_err(|e| {
                let _ =
                    self.effects
                        .deny(self.lease, prepared, ManagementDenialReasonV1::InvalidState);
                e.to_string()
            })?;
            if snapshot.state.project_id != project
                || Some(snapshot.state.session_id.as_str()) != change.request.change.session_id()
            {
                self.effects
                    .deny(
                        self.lease,
                        prepared,
                        ManagementDenialReasonV1::PermissionDenied,
                    )
                    .map_err(|e| e.to_string())?;
                return Err(failure("Archived source ownership mismatch"));
            }
        }
        let started = self
            .effects
            .start(self.lease, prepared)
            .map_err(|e| e.to_string())?;
        let receipt = match store.apply_change(&project, &change.request) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.effects
                    .mark_outcome_unknown(
                        self.lease,
                        &started,
                        ManagementUnknownReasonV1::UnclassifiedEffect,
                    )
                    .map_err(|e| e.to_string())?;
                // Only this exact local atomic batch can prove absence. Reconcile
                // is explicit; no attempt to run the body a second time.
                return Err(error.to_string());
            }
        };
        if let Some(plan) = removal
            && let Err(error) = plan.remove()
        {
            self.effects
                .mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                )
                .map_err(|e| e.to_string())?;
            return Err(format!(
                "Index deletion saved. Archived source deletion outcome is unknown: {error}"
            ));
        }
        let evidence = receipt_hash(&receipt)?;
        if self
            .effects
            .complete_started(self.lease, &started, evidence)
            .is_err()
        {
            if self.effects.effect(operation_id).map_err(|e|e.to_string())?.is_some_and(|e|matches!(e.state,ManagementEffectStateV1::Completed {evidence_hash} if evidence_hash==evidence)) { return Ok(Some(receipt)); }
            self.effects
                .mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                )
                .map_err(|e| e.to_string())?;
            return Err(failure(
                "Changes saved. Management receipt outcome is unknown",
            ));
        }
        Ok(Some(receipt))
    }

    /// Explicit reconciliation only. Never executes a batch or deletes files.
    pub fn reconcile(
        &self,
        store: &MemoryStore,
        operation: ManagementOperationIdV1,
    ) -> Result<MemoryChangeReceiptV1, String> {
        let project = self.bind(store)?;
        let effect = self
            .effects
            .effect(operation)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| failure("No management receipt"))?;
        let kind = match effect.request.inventory.encoded().as_str() {
            "C14" => MemoryChangeKindV1::Import,
            "C18" => MemoryChangeKindV1::Embed,
            "C19" => MemoryChangeKindV1::Index,
            "C20" => MemoryChangeKindV1::Forget,
            _ => return Err(failure("Unrelated management operation")),
        };
        if effect.request.scope.project != self.effects.project_scope_hash()
            || effect.request.scope.generation != 1
            || effect.request.scope.account_binding.is_some()
            || effect.request.scope.permission
                != if kind == MemoryChangeKindV1::Forget {
                    ManagementPermissionScopeV1::LocalSession
                } else {
                    ManagementPermissionScopeV1::Project
                }
            || effect.request.rewind != ManagementRewindDispositionV1::IrreversibleLocal
        {
            return Err(failure("Management receipt ownership mismatch"));
        }
        let receipt = store
            .change_receipt(&project, &operation.as_hex())
            .map_err(|e| e.to_string())?;
        let Some(receipt) = receipt else {
            if let Some(unknown) = self
                .effects
                .unknown_effect(operation)
                .map_err(|e| e.to_string())?
            {
                let evidence = ManagementEffectHashV1::digest(
                    format!(
                        "memory-atomic-receipt-absent-v1 {operation} {}",
                        effect.request.request_hash
                    )
                    .as_bytes(),
                );
                self.effects
                    .reconcile_abandon(self.lease, &unknown, evidence)
                    .map_err(|e| e.to_string())?;
            }
            return Err(failure(
                "No finalized memory change. Rerunning requires a new explicit operation",
            ));
        };
        if receipt.kind != kind
            || receipt.request_hash != effect.request.request_hash.as_hex()
            || receipt.before_revision != effect.request.scope.revision
        {
            return Err(failure("Memory receipt request mismatch"));
        }
        if kind == MemoryChangeKindV1::Forget {
            let session = receipt
                .session_id
                .as_deref()
                .ok_or_else(|| failure("Missing session in the deletion receipt"))?;
            validate_session_id(session).map_err(|e| e.to_string())?;
            if effect.request.scope.session
                != Some(ManagementEffectHashV1::digest(session.as_bytes()))
                || !store
                    .is_session_forgotten(&project, session)
                    .map_err(|e| e.to_string())?
            {
                return Err(failure("Deletion receipt ownership mismatch"));
            }
            if root_present(&self.data_root.join("sessions-v2").join(session))?
                || root_present(
                    &self
                        .effects
                        .state_dir()
                        .join("memory-archives")
                        .join(format!("{session}.archive")),
                )?
            {
                return Err(failure(
                    "Archived source deletion is incomplete. The outcome remains unknown",
                ));
            }
        }
        let evidence = receipt_hash(&receipt)?;
        match effect.state {
            ManagementEffectStateV1::Completed { evidence_hash } if evidence_hash == evidence => {
                Ok(receipt)
            }
            ManagementEffectStateV1::OutcomeUnknown { .. } => {
                let unknown = self
                    .effects
                    .unknown_effect(operation)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| failure("The unknown-outcome receipt changed"))?;
                self.effects
                    .complete_unknown(self.lease, &unknown, evidence)
                    .map_err(|e| e.to_string())?;
                Ok(receipt)
            }
            _ => Err(failure("The management operation is not reconcilable")),
        }
    }

    fn prepare_removal(&self, project: &str, session: &str) -> Result<RemovalPlan, String> {
        let protection = protected_snapshot()?;
        let mut plan = RemovalPlan { nodes: Vec::new() };
        let archive = self
            .effects
            .state_dir()
            .join("memory-archives")
            .join(format!("{session}.archive"));
        let first = plan.nodes.len();
        plan.collect_root(&archive, &protection)
            .map_err(|e| e.to_string())?;
        let archive_end = plan.nodes.len();
        plan.collect_root(
            &self.data_root.join("sessions-v2").join(session),
            &protection,
        )
        .map_err(|e| e.to_string())?;
        let mut total = 0_u64;
        for node in &plan.nodes[first..archive_end] {
            if node.directory {
                continue;
            }
            if node.path.extension().and_then(|s| s.to_str()) != Some("json") {
                return Err(failure("Archived sources contain an undefined file"));
            }
            total = total
                .checked_add(node.file.metadata().map_err(|e| e.to_string())?.len())
                .ok_or_else(|| failure("Archived sources exceed the limit"))?;
            if total > 8 * 1024 * 1024 {
                return Err(failure("Archived sources exceed the limit"));
            }
            let mut bytes = Vec::new();
            (&node.file)
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(failure("Archived sources exceed the limit"));
            }
            let json: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| failure("Cannot verify archived source ownership"))?;
            if json["version"] != 1
                || json["project_id"].as_str() != Some(project)
                || json["session_id"].as_str() != Some(session)
            {
                return Err(failure("Archived source project or session mismatch"));
            }
        }
        Ok(plan)
    }
}

fn receipt_hash(receipt: &MemoryChangeReceiptV1) -> Result<ManagementEffectHashV1, String> {
    serde_json::to_vec(receipt)
        .map(|bytes| ManagementEffectHashV1::digest(&bytes))
        .map_err(|e| e.to_string())
}

fn root_present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err(failure("The deletion target is not a regular directory"));
            }
            open_directory_nofollow(path).map_err(|e| e.to_string())?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut ancestor = path
                .parent()
                .ok_or_else(|| failure("Invalid deletion destination"))?;
            loop {
                match open_directory_nofollow(ancestor) {
                    Ok(_) => return Ok(false),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        ancestor = ancestor
                            .parent()
                            .ok_or_else(|| failure("Invalid deletion destination"))?
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Err(error) => Err(error.to_string()),
    }
}
fn open_child(parent: &File, name: &[u8], directory: bool) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC
                | if directory { libc::O_DIRECTORY } else { 0 },
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn named_identity(parent: &File, name: &[u8], metadata: &fs::Metadata) -> io::Result<()> {
    let name = CString::new(name).map_err(io::Error::other)?;
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
        return Err(io::Error::last_os_error());
    }
    let entry = unsafe { entry.assume_init() };
    if entry.st_dev as u64 != metadata.dev()
        || entry.st_ino != metadata.ino()
        || entry.st_mode & libc::S_IFMT == libc::S_IFLNK
    {
        return Err(io::Error::from_raw_os_error(libc::ESTALE));
    }
    Ok(())
}

struct RemovalNode {
    parent: File,
    file: File,
    name: Vec<u8>,
    path: PathBuf,
    directory: bool,
}
struct RemovalPlan {
    nodes: Vec<RemovalNode>,
}
impl RemovalPlan {
    fn collect_root(&mut self, path: &Path, protected: &ProtectedPathsSnapshot) -> io::Result<()> {
        if !root_present(path).map_err(io::Error::other)? {
            return Ok(());
        }
        let parent = open_directory_nofollow(
            path.parent()
                .ok_or_else(|| io::Error::other("invalid removal root"))?,
        )?;
        self.collect(
            &parent,
            path.file_name()
                .ok_or_else(|| io::Error::other("invalid removal name"))?
                .as_bytes(),
            path,
            protected,
            0,
        )
    }
    fn collect(
        &mut self,
        parent: &File,
        name: &[u8],
        path: &Path,
        protection: &ProtectedPathsSnapshot,
        depth: usize,
    ) -> io::Result<()> {
        if self.nodes.len() >= 4096 || depth > 32 {
            return Err(io::Error::other("removal inventory exceeds bounds"));
        }
        let file = open_child(parent, name, false)?;
        let meta = file.metadata()?;
        named_identity(parent, name, &meta)?;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7077 != 0
            || (!meta.is_dir() && (!meta.is_file() || meta.nlink() != 1))
            || protected(path, &meta, protection)
        {
            return Err(io::Error::other(
                "removal inventory contains protected or non-private entries",
            ));
        }
        let directory = meta.is_dir();
        let child_parent = if directory {
            Some(file.try_clone()?)
        } else {
            None
        };
        self.nodes.push(RemovalNode {
            parent: parent.try_clone()?,
            file,
            name: name.to_vec(),
            path: path.to_owned(),
            directory,
        });
        if let Some(parent) = child_parent {
            for name in directory_names_bounded(&parent, 4096)? {
                let path = path.join(&name);
                self.collect(&parent, name.as_bytes(), &path, protection, depth + 1)?;
            }
        }
        Ok(())
    }
    fn remove(self) -> io::Result<()> {
        // Validate the complete inventory before the first unlink. Every parent
        // is a pinned descriptor, and named identities are checked again below.
        let snapshot = protected_snapshot().map_err(io::Error::other)?;
        for node in &self.nodes {
            node.revalidate(&snapshot)?;
        }
        for node in self.nodes.iter().rev() {
            node.revalidate(&snapshot)?;
            let name = CString::new(node.name.clone()).map_err(io::Error::other)?;
            if unsafe {
                libc::unlinkat(
                    node.parent.as_raw_fd(),
                    name.as_ptr(),
                    if node.directory {
                        libc::AT_REMOVEDIR
                    } else {
                        0
                    },
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            node.parent.sync_all()?;
        }
        Ok(())
    }
}
impl RemovalNode {
    fn revalidate(&self, protection: &ProtectedPathsSnapshot) -> io::Result<()> {
        let parent_path = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("invalid parent"))?;
        let named_parent = open_directory_nofollow(parent_path)?;
        let current = named_parent.metadata()?;
        let pinned = self.parent.metadata()?;
        if (current.dev(), current.ino()) != (pinned.dev(), pinned.ino()) {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        let meta = self.file.metadata()?;
        named_identity(&self.parent, &self.name, &meta)?;
        if meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7077 != 0
            || (!self.directory && (!meta.is_file() || meta.nlink() != 1))
            || protected(&self.path, &meta, protection)
        {
            return Err(io::Error::other("removal target changed or is protected"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
