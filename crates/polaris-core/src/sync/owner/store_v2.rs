//! C10 bounded descriptor-relative store. Inspection and reservation write nothing.
//! The store borrows project EX; reservations own an opaque acquisition witness.

#[cfg(any(unix, test))]
use super::super::participation::ParticipationReadV2;
use super::super::participation::ParticipationSnapshotV2;
use super::super::remote_intent::*;
#[cfg(any(unix, test))]
use crate::management_effect::{
    ManagementEffectHashV1 as Hash, ManagementEffectStateV1, ManagementPermissionScopeV1,
    ManagementRewindDispositionV1,
};
use crate::management_effect::{ManagementEffectSnapshotV1, ManagementOperationIdV1 as Operation};
#[cfg(unix)]
use crate::project_mutation::open_directory_nofollow;
use crate::project_mutation::{ProjectMutationLeaseBindingV1, ProjectMutationLeaseV1};
use serde::Serialize;
#[cfg(any(unix, test))]
use std::ffi::CString;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    ffi::CStr,
    fs,
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
};

#[cfg(unix)]
const DIRECTORY: &str = "remote-intents-v2";
#[cfg(unix)]
const MAX_RECORD: u64 = MAX_SYNC_INTENT_BYTES_V2 as u64;
#[cfg(any(unix, test))]
const MAX_ACTIVE: usize = 256;
#[cfg(any(unix, test))]
const MAX_ACTIVE_BYTES: u64 = 32 * 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_TMP: usize = 32;
#[cfg(any(unix, test))]
const MAX_TMP_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_ARCHIVE: usize = 4096;
#[cfg(any(unix, test))]
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_ROOT_ENTRIES: usize = 289;
#[cfg(unix)]
const PAGE_SIZE: usize = 128;

#[derive(Debug, thiserror::Error)]
pub(crate) enum C10IntentStoreErrorV2 {
    #[error("c10_intent_corrupt")]
    Corrupt,
    #[error("c10_intent_blocked: {0}")]
    Blocked(&'static str),
    #[error("c10_intent_unavailable: {0}")]
    Unavailable(&'static str),
    #[error("c10_intent_storage")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Value(#[from] SyncIntentValueErrorV2),
}
type Result<T, E = C10IntentStoreErrorV2> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct Anchor {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
}
#[cfg(unix)]
impl Anchor {
    fn metadata(meta: &fs::Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            mode: meta.mode(),
            uid: meta.uid(),
        }
    }
    fn stat(meta: &libc::stat) -> Self {
        Self {
            device: meta.st_dev as u64,
            inode: meta.st_ino as u64,
            mode: meta.st_mode as u32,
            uid: meta.st_uid,
        }
    }
}
struct PinnedDirectory {
    file: File,
    anchor: Anchor,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct EntryEvidence {
    anchor: Anchor,
    length: u64,
    hash: SyncDigestV1,
}
struct Entry {
    history: SyncRemoteIntentHistoryV2,
    bytes: Vec<u8>,
    evidence: EntryEvidence,
    name: String,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Counts {
    active: usize,
    active_bytes: u64,
    temporary: usize,
    temporary_bytes: u64,
    archive: usize,
    archive_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum C10PendingIntentV2 {
    None,
    ExactNext {
        operation_id: Operation,
        sequence: u64,
        record_hash: SyncDigestV1,
    },
    Conflict,
}
struct Inventory {
    records: BTreeMap<Operation, Entry>,
    temporaries: BTreeMap<String, Entry>,
    evidence: BTreeMap<String, EntryEvidence>,
    directories: BTreeMap<String, Anchor>,
    counts: Counts,
    pending: C10PendingIntentV2,
}
impl Inventory {
    fn empty() -> Self {
        Self {
            records: BTreeMap::new(),
            temporaries: BTreeMap::new(),
            evidence: BTreeMap::new(),
            directories: BTreeMap::new(),
            counts: Counts::default(),
            pending: C10PendingIntentV2::None,
        }
    }
    fn digest(&self) -> SyncDigestV1 {
        SyncDigestV1::digest(
            &serde_json::to_vec(&(
                "polaris-c10-active-inventory-v2",
                &self.evidence,
                &self.directories,
            ))
            .expect("bounded primitive evidence"),
        )
    }
}

struct ArchiveInventory {
    evidence: BTreeMap<String, EntryEvidence>,
    buckets: BTreeMap<String, Anchor>,
    operations: BTreeSet<Operation>,
    count: usize,
    bytes: u64,
}
impl ArchiveInventory {
    fn digest(&self) -> SyncDigestV1 {
        SyncDigestV1::digest(
            &serde_json::to_vec(&(
                "polaris-c10-archive-inventory-v2",
                &self.evidence,
                &self.buckets,
            ))
            .expect("bounded primitive evidence"),
        )
    }
}

/// Non-cloneable reservation tied to an acquisition, independent of lease moves.
pub(crate) struct C10IntentReservationV2 {
    lease_binding: ProjectMutationLeaseBindingV1,
    operation_id: Operation,
    state_anchor: Anchor,
    snapshot: SyncDigestV1,
    archive_snapshot: SyncDigestV1,
}
/// Produced only from exact typed terminal readbacks, never a caller boolean.
pub(crate) struct C10TerminalReadbackV2 {
    operation_id: Operation,
    record_hash: SyncDigestV1,
    inventory_digest: SyncDigestV1,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct C10ArchiveCursorV2 {
    archive_digest: SyncDigestV1,
    after: String,
}
pub(crate) struct C10ArchivePageV2 {
    pub records: Vec<SyncRemoteIntentHistoryV2>,
    pub next: Option<C10ArchiveCursorV2>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum C10InitialLayoutV2 {
    Missing,
    EmptyRoot,
    ArchiveOnly,
    LockOnly,
    Complete,
}

pub(crate) struct C10IntentStoreV2<'a> {
    state_path: PathBuf,
    lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    state: PinnedDirectory,
    sync: Option<PinnedDirectory>,
    store: Option<PinnedDirectory>,
    archive: Option<PinnedDirectory>,
    owner_lock: Option<(File, Anchor)>,
    inventory: Inventory,
}

/// Read-only view. No mutable dereference or publication API is exposed.
pub(crate) struct C10IntentReadV2<'a>(C10IntentStoreV2<'a>);
impl<'a> std::ops::Deref for C10IntentReadV2<'a> {
    type Target = C10IntentStoreV2<'a>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<'a> C10IntentReadV2<'a> {
    #[cfg(unix)]
    pub(crate) fn inspect(
        state: &Path,
        lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    ) -> Result<Self> {
        let mut value = C10IntentStoreV2::pin_read(state, lease)?;
        value.inventory = value.scan_active()?;
        Ok(Self(value))
    }
}

impl<'a> C10IntentStoreV2<'a> {
    /// Reads every active entry and classifies pending evidence without repair.
    #[cfg(unix)]
    pub(crate) fn inspect_at_state_dir(
        state: &Path,
        lease: &'a ProjectMutationLeaseV1,
    ) -> Result<Self> {
        let mut value = Self::pin(state, lease)?;
        value.inventory = value.scan_active()?;
        Ok(value)
    }

    /// Direct lookup deliberately does not depend on a successful blocker scan.
    #[cfg(unix)]
    pub(crate) fn direct_lookup_at_state_dir(
        state: &Path,
        lease: &'a ProjectMutationLeaseV1,
        operation: Operation,
    ) -> Result<Option<SyncRemoteIntentHistoryV2>> {
        Self::pin(state, lease)?.lookup(operation)
    }

    #[cfg(unix)]
    pub(crate) fn archive_page_at_state_dir(
        state: &Path,
        lease: &'a ProjectMutationLeaseV1,
        cursor: Option<&C10ArchiveCursorV2>,
    ) -> Result<C10ArchivePageV2> {
        Self::pin(state, lease)?.archive_page(cursor)
    }

    #[cfg(unix)]
    fn pin(state: &Path, lease: &'a ProjectMutationLeaseV1) -> Result<Self> {
        Self::pin_read(
            state,
            crate::project_mutation::ProjectReadLeaseV1::Mutation(lease),
        )
    }
    #[cfg(unix)]
    fn pin_read(
        state: &Path,
        lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    ) -> Result<Self> {
        lease
            .verify(state)
            .map_err(|_| C10IntentStoreErrorV2::Blocked("project read lease"))?;
        if !state.is_absolute()
            || fs::canonicalize(state)? != state
            || lease.store_root() != state.join("rewind-workspace")
        {
            return Err(C10IntentStoreErrorV2::Blocked("project lease identity"));
        }
        let lease_file = open_directory_nofollow(lease.store_root())?;
        let lease_meta = lease_file.metadata()?;
        private_directory(&lease_meta)?;
        if (lease_meta.dev(), lease_meta.ino()) != lease.store_identity() {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let file = open_directory_nofollow(state)?;
        let meta = file.metadata()?;
        private_directory(&meta)?;
        let state_directory = PinnedDirectory {
            file,
            anchor: Anchor::metadata(&meta),
        };
        let sync = open_directory(&state_directory.file, "sync")?;
        let store = match &sync {
            Some(sync) => open_directory(&sync.file, DIRECTORY)?,
            None => None,
        };
        let (archive, owner_lock) = if let Some(store) = &store {
            let archive = open_directory(&store.file, "archive")?;
            let name = component("owner.lock")?;
            let lock = match open_at(&store.file, &name, libc::O_RDONLY) {
                Ok(lock) => {
                    let meta = lock.metadata()?;
                    private_file(&meta)?;
                    if meta.len() != 0 {
                        return Err(C10IntentStoreErrorV2::Corrupt);
                    }
                    let anchor = Anchor::metadata(&meta);
                    verify_named(&store.file, &name, anchor)?;
                    lock_exclusive(&lock)?;
                    verify_named(&store.file, &name, anchor)?;
                    Some((lock, anchor))
                }
                Err(C10IntentStoreErrorV2::Io(error))
                    if error.kind() == io::ErrorKind::NotFound
                        && named_stat(&store.file, &name)?.is_none() =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            (archive, lock)
        } else {
            (None, None)
        };
        let result = Self {
            state_path: state.to_owned(),
            lease,
            state: state_directory,
            sync,
            store,
            archive,
            owner_lock,
            inventory: Inventory::empty(),
        };
        result.verify_tree()?;
        result.require_empty_partial_layout()?;
        Ok(result)
    }

    pub(crate) fn initial_layout(&self) -> C10InitialLayoutV2 {
        match (&self.store, &self.archive, &self.owner_lock) {
            (None, _, _) => C10InitialLayoutV2::Missing,
            (Some(_), None, None) => C10InitialLayoutV2::EmptyRoot,
            (Some(_), Some(_), None) => C10InitialLayoutV2::ArchiveOnly,
            (Some(_), None, Some(_)) => C10InitialLayoutV2::LockOnly,
            (Some(_), Some(_), Some(_)) => C10InitialLayoutV2::Complete,
        }
    }

    #[cfg(unix)]
    fn require_empty_partial_layout(&self) -> Result<()> {
        if self.initial_layout() == C10InitialLayoutV2::Complete || self.store.is_none() {
            return Ok(());
        }
        let store = self.store.as_ref().ok_or(C10IntentStoreErrorV2::Corrupt)?;
        let mut expected = Vec::new();
        if self.archive.is_some() {
            expected.push("archive".to_owned());
        }
        if self.owner_lock.is_some() {
            expected.push("owner.lock".to_owned());
        }
        if directory_names(&store.file, 2)? != expected {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        if let Some(archive) = &self.archive {
            directory_names(&archive.file, 0)?;
        }
        Ok(())
    }

    /// P3 explicitly completes a verified empty initial layout under its EX lease.
    #[cfg(unix)]
    pub(crate) fn complete_initial_layout(
        &mut self,
        reservation: &mut C10IntentReservationV2,
    ) -> Result<()> {
        self.validate_reservation(reservation, reservation.operation_id)?;
        if self.initial_layout() == C10InitialLayoutV2::Complete {
            return Ok(());
        }
        self.initialize_for_publication()?;
        let initialized = self.scan_active()?;
        let archive = self.scan_archive()?;
        if !initialized.records.is_empty()
            || !initialized.temporaries.is_empty()
            || archive.count != 0
            || !archive.buckets.is_empty()
        {
            return Err(C10IntentStoreErrorV2::Blocked(
                "initialization inventory changed",
            ));
        }
        self.inventory = initialized;
        reservation.snapshot = self.inventory.digest();
        reservation.archive_snapshot = archive.digest();
        Ok(())
    }

    pub(crate) fn pending(&self) -> &C10PendingIntentV2 {
        &self.inventory.pending
    }

    /// P3 checks the complete management request before explicitly resuming it.
    /// Readback preserves every temporary and grants no publication capability.
    #[cfg(unix)]
    pub(crate) fn pending_history_for_operation(
        &self,
        operation: Operation,
    ) -> Result<Option<SyncRemoteIntentHistoryV2>> {
        let current = self.scan_active()?;
        if current.digest() != self.inventory.digest() {
            return Err(C10IntentStoreErrorV2::Blocked("pending inventory changed"));
        }
        match current.pending {
            C10PendingIntentV2::None => {}
            C10PendingIntentV2::ExactNext { operation_id, .. } if operation_id == operation => {}
            _ => return Err(C10IntentStoreErrorV2::Blocked("pending operation mismatch")),
        }
        let readback = self.scan_active()?;
        if readback.digest() != current.digest() || readback.pending != current.pending {
            return Err(C10IntentStoreErrorV2::Blocked("pending readback changed"));
        }
        let C10PendingIntentV2::ExactNext {
            operation_id,
            sequence,
            record_hash,
        } = current.pending
        else {
            return Ok(None);
        };
        let temporary = current
            .temporaries
            .values()
            .next()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        let repeated = readback
            .temporaries
            .get(&temporary.name)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if current.temporaries.len() != 1
            || readback.temporaries.len() != 1
            || temporary.history.operation_id() != operation_id
            || temporary.history.current().sequence() != sequence
            || temporary.history.current().record_hash() != record_hash
            || repeated.evidence != temporary.evidence
            || repeated.bytes != temporary.bytes
            || repeated.history != temporary.history
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        self.verify_tree()?;
        Ok(Some(repeated.history.clone()))
    }

    pub(crate) fn active_records(&self) -> impl Iterator<Item = &SyncRemoteIntentHistoryV2> {
        self.inventory.records.values().map(|entry| &entry.history)
    }
    pub(crate) fn inventory_digest(&self) -> SyncDigestV1 {
        self.inventory.digest()
    }

    #[cfg(unix)]
    pub(crate) fn lookup(&self, operation: Operation) -> Result<Option<SyncRemoteIntentHistoryV2>> {
        self.verify_tree()?;
        let name = record_name(operation);
        let active = match &self.store {
            Some(store) => read_entry(&store.file, &name)?,
            None => None,
        };
        let archived = self.read_archive_entry(&name)?;
        if active.is_some() && archived.is_some() {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let entry = active.or(archived);
        if entry
            .as_ref()
            .is_some_and(|entry| entry.history.operation_id() != operation)
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        self.verify_tree()?;
        Ok(entry.map(|entry| entry.history))
    }

    /// Reads a historical terminal pair from its actual archive location.
    /// Current participation belongs to active readback, never this archive proof.
    #[cfg(unix)]
    pub(crate) fn archived_terminal_pair_readback(
        &self,
        operation: Operation,
        management: &ManagementEffectSnapshotV1,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        self.verify_tree()?;
        let name = record_name(operation);
        let store = self.store.as_ref().ok_or(C10IntentStoreErrorV2::Corrupt)?;
        let archived = self
            .read_archive_entry(&name)?
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if archived.history.operation_id() != operation || read_entry(&store.file, &name)?.is_some()
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        terminal_management_pair(&archived.history, management)?;
        self.verify_tree()?;
        let readback = self
            .read_archive_entry(&name)?
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if readback.evidence != archived.evidence
            || readback.bytes != archived.bytes
            || read_entry(&store.file, &name)?.is_some()
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        self.verify_tree()?;
        Ok(archived.history)
    }

    /// Reads the actual active terminal pair without reserving mutation capacity.
    /// This validates current participation and grants no archive write capability.
    #[cfg(unix)]
    pub(crate) fn active_terminal_pair_readback(
        &self,
        operation: Operation,
        management: &ManagementEffectSnapshotV1,
        participation: &ParticipationSnapshotV2,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        self.verify_tree()?;
        let name = record_name(operation);
        let store = self.store.as_ref().ok_or(C10IntentStoreErrorV2::Corrupt)?;
        let active = read_entry(&store.file, &name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if active.history.operation_id() != operation
            || !active.history.is_terminal()
            || self.read_archive_entry(&name)?.is_some()
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        active_terminal_pair(&active.history, management, participation)?;
        self.verify_tree()?;
        let readback = read_entry(&store.file, &name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if readback.evidence != active.evidence
            || readback.bytes != active.bytes
            || self.read_archive_entry(&name)?.is_some()
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        self.verify_tree()?;
        Ok(active.history)
    }

    #[cfg(unix)]
    pub(crate) fn archive_inventory_digest(&self) -> Result<SyncDigestV1> {
        Ok(self.scan_archive()?.digest())
    }
    #[cfg(unix)]
    pub(crate) fn verify_read_inventory(&self, archive: SyncDigestV1) -> Result<()> {
        if self.scan_active()?.digest() != self.inventory.digest()
            || self.scan_archive()?.digest() != archive
        {
            return Err(C10IntentStoreErrorV2::Blocked("read inventory changed"));
        }
        self.verify_tree()
    }

    /// Reads actual stored participation without claiming a live credential.
    #[cfg(unix)]
    pub(crate) fn active_durable_terminal_pair_readback(
        &self,
        operation: Operation,
        management: &ManagementEffectSnapshotV1,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        let snapshot = super::super::participation::read_snapshot(&self.state_path)
            .map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
        let ParticipationReadV2::StaleCredentialBindingOnly(part) = &snapshot.classification else {
            return Err(C10IntentStoreErrorV2::Blocked(
                "stored participation terminal pair",
            ));
        };
        self.verify_tree()?;
        let store = self.store.as_ref().ok_or(C10IntentStoreErrorV2::Corrupt)?;
        let name = record_name(operation);
        let first = read_entry(&store.file, &name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if first.history.operation_id() != operation
            || !first.history.is_terminal()
            || self.read_archive_entry(&name)?.is_some()
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        durable_participation_pair(&first.history, management, part, snapshot.exact_digest)?;
        self.verify_tree()?;
        let fresh = read_entry(&store.file, &name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if first.evidence != fresh.evidence
            || first.bytes != fresh.bytes
            || self.read_archive_entry(&name)?.is_some()
            || super::super::participation::read_snapshot(&self.state_path)
                .map_err(|_| C10IntentStoreErrorV2::Corrupt)?
                != snapshot
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        self.verify_tree()?;
        Ok(first.history)
    }

    #[cfg(unix)]
    pub(crate) fn reserve_operation(
        &mut self,
        operation: Operation,
    ) -> Result<C10IntentReservationV2> {
        self.lease
            .mutation()
            .map_err(|_| C10IntentStoreErrorV2::Blocked("exclusive project lease required"))?;
        self.refresh_exact()?;
        if self.inventory.pending != C10PendingIntentV2::None {
            return Err(C10IntentStoreErrorV2::Blocked("pending intent"));
        }
        let archive = self.scan_archive()?;
        if archive.operations.contains(&operation) {
            return Err(C10IntentStoreErrorV2::Blocked(
                "archived operation immutable",
            ));
        }
        let existing = self.inventory.records.get(&operation);
        let mut counts = self.inventory.counts;
        counts.active = counts
            .active
            .checked_add(usize::from(existing.is_none()))
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        counts.active_bytes = counts
            .active_bytes
            .checked_sub(existing.map_or(0, |entry| entry.bytes.len() as u64))
            .and_then(|sum| sum.checked_add(MAX_RECORD))
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        counts.temporary = counts
            .temporary
            .checked_add(1)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        counts.temporary_bytes = counts
            .temporary_bytes
            .checked_add(MAX_RECORD)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        counts.archive = archive
            .count
            .checked_add(1)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        counts.archive_bytes = archive
            .bytes
            .checked_add(MAX_RECORD)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        capacity(counts)?;
        // The fixed archive root and a possible new two-hex bucket also have
        // explicit directory headroom. A full 256-bucket root already has every
        // possible bucket; it needs no new entry.
        if let Some(directory) = &self.archive {
            let names = directory_names(&directory.file, 256)?;
            let prefix = &record_name(operation)[..2];
            if !names.iter().any(|name| name == prefix) && names.len() == 256 {
                return Err(C10IntentStoreErrorV2::Blocked("archive bucket capacity"));
            }
        }
        Ok(C10IntentReservationV2 {
            lease_binding: self
                .lease
                .mutation()
                .map_err(|_| C10IntentStoreErrorV2::Blocked("exclusive project lease required"))?
                .bind_identity(),
            operation_id: operation,
            state_anchor: self.state.anchor,
            snapshot: self.inventory.digest(),
            archive_snapshot: archive.digest(),
        })
    }

    #[cfg(unix)]
    pub(crate) fn publish(
        &mut self,
        reservation: &mut C10IntentReservationV2,
        history: &SyncRemoteIntentHistoryV2,
    ) -> Result<()> {
        self.validate_reservation(reservation, history.operation_id())?;
        let bytes = history.canonical_bytes()?;
        if self.inventory.pending != C10PendingIntentV2::None {
            return Err(C10IntentStoreErrorV2::Blocked("pending intent"));
        }
        if let Some(prior) = self.inventory.records.get(&history.operation_id()) {
            if prior.bytes == bytes {
                return Ok(());
            }
            history.is_exact_extension_of(&prior.history)?;
        } else {
            if history.records().len() != 1 {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            if let SyncRemoteIntentStateV2::ReconciledAbandon { evidence } =
                history.current().state()
            {
                let SyncAbandonEvidenceV2::NoIntent { evidence } = evidence.as_ref() else {
                    return Err(C10IntentStoreErrorV2::Corrupt);
                };
                if evidence.active_inventory_digest != reservation.snapshot {
                    return Err(C10IntentStoreErrorV2::Corrupt);
                }
            }
        }
        if self.initial_layout() != C10InitialLayoutV2::Complete {
            self.initialize_for_publication()?;
            let initialized = self.scan_active()?;
            let archive = self.scan_archive()?;
            if !initialized.records.is_empty()
                || !initialized.temporaries.is_empty()
                || archive.count != 0
                || !archive.buckets.is_empty()
            {
                return Err(C10IntentStoreErrorV2::Blocked(
                    "initialization inventory changed",
                ));
            }
            self.inventory = initialized;
            reservation.snapshot = self.inventory.digest();
            reservation.archive_snapshot = archive.digest();
        } else {
            self.validate_reservation(reservation, history.operation_id())?;
        }
        let directory = &self
            .store
            .as_ref()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?
            .file;
        let target = record_name(history.operation_id());
        let temporary_name = temporary_name(&target, history.current().sequence())?;
        let temporary_component = component(&temporary_name)?;
        fault_point(5)?;
        let mut temporary = open_at(
            directory,
            &temporary_component,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        private_file(&temporary.metadata()?)?;
        let anchor = Anchor::metadata(&temporary.metadata()?);
        verify_named(directory, &temporary_component, anchor)?;
        fault_point(6)?;
        temporary.write_all(&bytes)?;
        fault_point(7)?;
        temporary.sync_all()?;
        fault_point(8)?;
        self.verify_tree()?;
        let pending = self.scan_active()?;
        let mut expected = self.inventory.evidence.clone();
        expected.insert(
            temporary_name.clone(),
            EntryEvidence {
                anchor,
                length: bytes.len() as u64,
                hash: SyncDigestV1::digest(&bytes),
            },
        );
        if pending.evidence != expected
            || pending.temporaries.len() != 1
            || !matches!(pending.pending, C10PendingIntentV2::ExactNext { operation_id, sequence, record_hash } if operation_id == history.operation_id() && sequence == history.current().sequence() && record_hash == history.current().record_hash())
        {
            return Err(C10IntentStoreErrorV2::Blocked(
                "publication inventory changed",
            ));
        }
        self.publish_pending_entry(history.operation_id(), &pending)?;
        reservation.snapshot = self.inventory.digest();
        Ok(())
    }

    /// P3 invokes this only after validating the same management recovery pair.
    /// Inspection itself never publishes or removes a temporary.
    #[cfg(unix)]
    pub(crate) fn resume_pending(
        &mut self,
        operation: Operation,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        self.lease
            .mutation()
            .map_err(|_| C10IntentStoreErrorV2::Blocked("exclusive project lease required"))?;
        self.refresh_exact()?;
        match self.inventory.pending {
            C10PendingIntentV2::ExactNext { operation_id, .. } if operation_id == operation => {}
            _ => return Err(C10IntentStoreErrorV2::Blocked("pending operation mismatch")),
        }
        let inventory = self.scan_active()?;
        if inventory.digest() != self.inventory.digest() {
            return Err(C10IntentStoreErrorV2::Blocked("pending inventory changed"));
        }
        let result = inventory
            .temporaries
            .values()
            .next()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?
            .history
            .clone();
        self.publish_pending_entry(operation, &inventory)?;
        Ok(result)
    }

    #[cfg(unix)]
    fn publish_pending_entry(&mut self, operation: Operation, pending: &Inventory) -> Result<()> {
        self.lease
            .mutation()
            .map_err(|_| C10IntentStoreErrorV2::Blocked("exclusive project lease required"))?;
        self.verify_tree()?;
        let temporary = pending
            .temporaries
            .values()
            .next()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if pending.temporaries.len() != 1 || temporary.history.operation_id() != operation {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let directory = &self
            .store
            .as_ref()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?
            .file;
        let target = record_name(operation);
        let temporary_component = component(&temporary.name)?;
        verify_named(directory, &temporary_component, temporary.evidence.anchor)?;
        let held = open_at(directory, &temporary_component, libc::O_RDONLY)?;
        if bounded_bytes(&held, MAX_RECORD)? != temporary.bytes {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        held.sync_all()?;
        let target_component = component(&target)?;
        if let Some(prior) = pending.records.get(&operation) {
            temporary.history.is_exact_extension_of(&prior.history)?;
            verify_named(directory, &target_component, prior.evidence.anchor)?;
            rename_replace(
                directory,
                &temporary_component,
                directory,
                &target_component,
            )?;
        } else {
            if temporary.history.records().len() != 1 {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            rename_no_replace(
                directory,
                &temporary_component,
                directory,
                &target_component,
            )?;
        }
        fault_point(9)?;
        directory.sync_all()?;
        fault_point(10)?;
        self.verify_tree()?;
        verify_named(directory, &target_component, temporary.evidence.anchor)?;
        let mut expected = pending.evidence.clone();
        expected.remove(&temporary.name);
        expected.insert(target.clone(), temporary.evidence.clone());
        let published = self.scan_active()?;
        if published.evidence != expected
            || published
                .records
                .get(&operation)
                .is_none_or(|entry| entry.bytes != temporary.bytes)
        {
            return Err(C10IntentStoreErrorV2::Blocked("whole publication readback"));
        }
        self.inventory = published;
        Ok(())
    }

    /// Seals exact typed evidence after P3 has completed durable live readbacks.
    #[cfg(unix)]
    pub(crate) fn terminal_readback(
        &mut self,
        reservation: &C10IntentReservationV2,
        management: &ManagementEffectSnapshotV1,
        participation: &ParticipationSnapshotV2,
    ) -> Result<C10TerminalReadbackV2> {
        self.validate_reservation(reservation, reservation.operation_id)?;
        let history = &self
            .inventory
            .records
            .get(&reservation.operation_id)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?
            .history;
        active_terminal_pair(history, management, participation)?;
        Ok(C10TerminalReadbackV2 {
            operation_id: reservation.operation_id,
            record_hash: history.current().record_hash(),
            inventory_digest: self.inventory.digest(),
        })
    }

    /// P3 alone owns the call after management/receipt/participation readbacks.
    #[cfg(unix)]
    pub(crate) fn archive_terminal(
        &mut self,
        reservation: &mut C10IntentReservationV2,
        completion: C10TerminalReadbackV2,
    ) -> Result<()> {
        self.validate_reservation(reservation, completion.operation_id)?;
        if completion.inventory_digest != self.inventory.digest() {
            return Err(C10IntentStoreErrorV2::Blocked("stale terminal readback"));
        }
        let entry = self
            .inventory
            .records
            .get(&completion.operation_id)
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if !entry.history.is_terminal()
            || entry.history.current().record_hash() != completion.record_hash
        {
            return Err(C10IntentStoreErrorV2::Blocked("terminal record identity"));
        }
        let source_evidence = entry.evidence.clone();
        let source_bytes = entry.bytes.clone();
        let name = entry.name.clone();
        let before = self.scan_archive()?;
        if before.count.checked_add(1).is_none_or(|n| n > MAX_ARCHIVE)
            || before
                .bytes
                .checked_add(source_bytes.len() as u64)
                .is_none_or(|n| n > MAX_ARCHIVE_BYTES)
        {
            return Err(C10IntentStoreErrorV2::Blocked("archive capacity"));
        }
        let archive = self
            .archive
            .as_ref()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        let prefix = &name[..2];
        let bucket = match open_directory(&archive.file, prefix)? {
            Some(bucket) => bucket,
            None => {
                fault_point(11)?;
                let bucket = create_directory(&archive.file, prefix)?;
                archive.file.sync_all()?;
                bucket
            }
        };
        let source = &self
            .store
            .as_ref()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?
            .file;
        let component = component(&name)?;
        verify_named(source, &component, source_evidence.anchor)?;
        rename_no_replace(source, &component, &bucket.file, &component)?;
        fault_point(12)?;
        bucket.file.sync_all()?;
        fault_point(13)?;
        source.sync_all()?;
        fault_point(14)?;
        self.verify_tree()?;
        verify_named(&archive.file, &crate_component(prefix)?, bucket.anchor)?;
        verify_named(&bucket.file, &component, source_evidence.anchor)?;
        let readback = read_entry(&bucket.file, &name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if readback.bytes != source_bytes || readback.evidence != source_evidence {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let mut expected_active = self.inventory.evidence.clone();
        expected_active.remove(&name);
        let active = self.scan_active()?;
        if active.evidence != expected_active {
            return Err(C10IntentStoreErrorV2::Blocked("archive active readback"));
        }
        let archived = self.scan_archive()?;
        let mut expected_archive = before.evidence.clone();
        expected_archive.insert(format!("{prefix}/{name}"), source_evidence);
        let mut expected_buckets = before.buckets.clone();
        expected_buckets.insert(prefix.to_owned(), bucket.anchor);
        if archived.evidence != expected_archive || archived.buckets != expected_buckets {
            return Err(C10IntentStoreErrorV2::Blocked("archive whole readback"));
        }
        self.inventory = active;
        reservation.snapshot = self.inventory.digest();
        reservation.archive_snapshot = archived.digest();
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn archive_page(
        &self,
        cursor: Option<&C10ArchiveCursorV2>,
    ) -> Result<C10ArchivePageV2> {
        let archive = self.scan_archive()?;
        let digest = archive.digest();
        if cursor.is_some_and(|cursor| {
            cursor.archive_digest != digest || !archive.evidence.contains_key(&cursor.after)
        }) {
            return Err(C10IntentStoreErrorV2::Blocked("stale archive cursor"));
        }
        let names: Vec<_> = archive
            .evidence
            .keys()
            .filter(|name| cursor.is_none_or(|cursor| *name > &cursor.after))
            .take(PAGE_SIZE + 1)
            .cloned()
            .collect();
        let mut records = Vec::new();
        for key in names.iter().take(PAGE_SIZE) {
            let (_, name) = key.split_once('/').ok_or(C10IntentStoreErrorV2::Corrupt)?;
            records.push(
                self.read_archive_entry(name)?
                    .ok_or(C10IntentStoreErrorV2::Corrupt)?
                    .history,
            );
        }
        if self.scan_archive()?.digest() != digest {
            return Err(C10IntentStoreErrorV2::Blocked("archive page changed"));
        }
        let next = (names.len() > PAGE_SIZE).then(|| C10ArchiveCursorV2 {
            archive_digest: digest,
            after: names[PAGE_SIZE - 1].clone(),
        });
        Ok(C10ArchivePageV2 { records, next })
    }

    #[cfg(unix)]
    fn validate_reservation(
        &mut self,
        reservation: &C10IntentReservationV2,
        operation: Operation,
    ) -> Result<()> {
        reservation
            .lease_binding
            .require_same_lease(
                self.lease.mutation().map_err(|_| {
                    C10IntentStoreErrorV2::Blocked("exclusive project lease required")
                })?,
            )
            .map_err(|_| C10IntentStoreErrorV2::Blocked("reservation lease acquisition"))?;
        if reservation.operation_id != operation || reservation.state_anchor != self.state.anchor {
            return Err(C10IntentStoreErrorV2::Blocked("reservation identity"));
        }
        self.refresh_exact()?;
        if self.inventory.digest() != reservation.snapshot
            || self.scan_archive()?.digest() != reservation.archive_snapshot
        {
            return Err(C10IntentStoreErrorV2::Blocked(
                "reservation inventory changed",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn refresh_exact(&mut self) -> Result<()> {
        let current = self.scan_active()?;
        if current.digest() != self.inventory.digest() {
            return Err(C10IntentStoreErrorV2::Blocked("active inventory changed"));
        }
        self.inventory = current;
        Ok(())
    }

    #[cfg(unix)]
    fn verify_tree(&self) -> Result<()> {
        self.lease
            .verify(&self.state_path)
            .map_err(|_| C10IntentStoreErrorV2::Blocked("project read lease"))?;
        let current = open_directory_nofollow(&self.state_path)?;
        private_directory(&current.metadata()?)?;
        if Anchor::metadata(&current.metadata()?) != self.state.anchor
            || Anchor::metadata(&self.state.file.metadata()?) != self.state.anchor
        {
            return Err(C10IntentStoreErrorV2::Blocked("state anchor changed"));
        }
        let lease = open_directory_nofollow(self.lease.store_root())?;
        private_directory(&lease.metadata()?)?;
        if (lease.metadata()?.dev(), lease.metadata()?.ino()) != self.lease.store_identity() {
            return Err(C10IntentStoreErrorV2::Blocked("lease anchor changed"));
        }
        verify_optional_directory(&self.state.file, "sync", self.sync.as_ref())?;
        if let Some(sync) = &self.sync {
            verify_optional_directory(&sync.file, DIRECTORY, self.store.as_ref())?;
        }
        if let Some(store) = &self.store {
            verify_optional_directory(&store.file, "archive", self.archive.as_ref())?;
            if let Some((lock, anchor)) = &self.owner_lock {
                if bounded_bytes(lock, 0)?.len() != 0
                    || Anchor::metadata(&lock.metadata()?) != *anchor
                {
                    return Err(C10IntentStoreErrorV2::Corrupt);
                }
                verify_named(&store.file, &component("owner.lock")?, *anchor)?;
            } else if named_stat(&store.file, &component("owner.lock")?)?.is_some() {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
        } else if self.archive.is_some() || self.owner_lock.is_some() {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn scan_active(&self) -> Result<Inventory> {
        self.verify_tree()?;
        let mut inventory = Inventory::empty();
        inventory
            .directories
            .insert("state".into(), self.state.anchor);
        if let Some(sync) = &self.sync {
            inventory.directories.insert("sync".into(), sync.anchor);
        }
        if let Some(store) = &self.store {
            inventory.directories.insert("store".into(), store.anchor);
        }
        if let Some(archive) = &self.archive {
            inventory
                .directories
                .insert("archive".into(), archive.anchor);
        }
        let Some(store) = &self.store else {
            return Ok(inventory);
        };
        if self.initial_layout() != C10InitialLayoutV2::Complete {
            self.require_empty_partial_layout()?;
            if let Some((_, anchor)) = &self.owner_lock {
                inventory.evidence.insert(
                    "owner.lock".into(),
                    EntryEvidence {
                        anchor: *anchor,
                        length: 0,
                        hash: SyncDigestV1::digest(&[]),
                    },
                );
            }
            self.verify_tree()?;
            return Ok(inventory);
        }
        let names = directory_names(&store.file, MAX_ROOT_ENTRIES)?;
        if !names.iter().any(|name| name == "owner.lock")
            || !names.iter().any(|name| name == "archive")
        {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let lock = self
            .owner_lock
            .as_ref()
            .ok_or(C10IntentStoreErrorV2::Corrupt)?;
        inventory.evidence.insert(
            "owner.lock".into(),
            EntryEvidence {
                anchor: lock.1,
                length: 0,
                hash: SyncDigestV1::digest(&[]),
            },
        );
        for name in &names {
            if name == "owner.lock" || name == "archive" {
                continue;
            }
            let kind = classify_name(name)?;
            let entry = read_entry(&store.file, name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
            let target = record_name(entry.history.operation_id());
            match kind {
                NameKind::Record => {
                    if target != *name
                        || self.read_archive_entry(name)?.is_some()
                        || inventory
                            .records
                            .contains_key(&entry.history.operation_id())
                    {
                        return Err(C10IntentStoreErrorV2::Corrupt);
                    }
                    inventory.counts.active += 1;
                    inventory.counts.active_bytes =
                        add_bytes(inventory.counts.active_bytes, &entry.bytes)?;
                    inventory.records.insert(
                        entry.history.operation_id(),
                        Entry {
                            history: entry.history.clone(),
                            bytes: entry.bytes.clone(),
                            evidence: entry.evidence.clone(),
                            name: entry.name.clone(),
                        },
                    );
                }
                NameKind::Temporary {
                    target: named_target,
                    sequence,
                } => {
                    if named_target != target
                        || sequence != entry.history.current().sequence()
                        || self.read_archive_entry(&target)?.is_some()
                    {
                        return Err(C10IntentStoreErrorV2::Corrupt);
                    }
                    inventory.counts.temporary += 1;
                    inventory.counts.temporary_bytes =
                        add_bytes(inventory.counts.temporary_bytes, &entry.bytes)?;
                    inventory.temporaries.insert(
                        name.clone(),
                        Entry {
                            history: entry.history.clone(),
                            bytes: entry.bytes.clone(),
                            evidence: entry.evidence.clone(),
                            name: entry.name.clone(),
                        },
                    );
                }
            }
            inventory.evidence.insert(name.clone(), entry.evidence);
        }
        capacity(inventory.counts)?;
        for temporary in inventory.temporaries.values() {
            if let Some(prior) = inventory.records.get(&temporary.history.operation_id()) {
                temporary.history.is_exact_extension_of(&prior.history)?;
            } else if temporary.history.records().len() != 1
                || temporary.history.current().sequence() != 1
                || temporary.history.current().previous_record_hash() != zero_digest_v2()
            {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
        }
        inventory.pending = match inventory.temporaries.len() {
            0 => C10PendingIntentV2::None,
            1 => {
                let current = inventory
                    .temporaries
                    .values()
                    .next()
                    .expect("one temporary")
                    .history
                    .current();
                C10PendingIntentV2::ExactNext {
                    operation_id: current.identity().operation_id,
                    sequence: current.sequence(),
                    record_hash: current.record_hash(),
                }
            }
            _ => C10PendingIntentV2::Conflict,
        };
        for (name, evidence) in &inventory.evidence {
            verify_named(&store.file, &component(name)?, evidence.anchor)?;
            let file = open_at(&store.file, &component(name)?, libc::O_RDONLY)?;
            let bytes = bounded_bytes(&file, if name == "owner.lock" { 0 } else { MAX_RECORD })?;
            if bytes.len() as u64 != evidence.length
                || SyncDigestV1::digest(&bytes) != evidence.hash
            {
                return Err(C10IntentStoreErrorV2::Blocked("active readback changed"));
            }
        }
        if directory_names(&store.file, MAX_ROOT_ENTRIES)? != names {
            return Err(C10IntentStoreErrorV2::Blocked("active names changed"));
        }
        self.verify_tree()?;
        Ok(inventory)
    }

    #[cfg(unix)]
    fn read_archive_entry(&self, name: &str) -> Result<Option<Entry>> {
        if !is_record_name(name) {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let Some(archive) = &self.archive else {
            return Ok(None);
        };
        let prefix = &name[..2];
        let Some(bucket) = open_directory(&archive.file, prefix)? else {
            return Ok(None);
        };
        let entry = read_entry(&bucket.file, name)?;
        if entry.as_ref().is_some_and(|entry| {
            !entry.history.is_terminal() || record_name(entry.history.operation_id()) != name
        }) {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        verify_named(&archive.file, &component(prefix)?, bucket.anchor)?;
        Ok(entry)
    }

    #[cfg(unix)]
    fn scan_archive(&self) -> Result<ArchiveInventory> {
        self.verify_tree()?;
        let mut inventory = ArchiveInventory {
            evidence: BTreeMap::new(),
            buckets: BTreeMap::new(),
            operations: BTreeSet::new(),
            count: 0,
            bytes: 0,
        };
        let Some(archive) = &self.archive else {
            return Ok(inventory);
        };
        let buckets = directory_names(&archive.file, 256)?;
        for prefix in &buckets {
            if !lower_hex(prefix, 2) {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            let bucket =
                open_directory(&archive.file, prefix)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
            inventory.buckets.insert(prefix.clone(), bucket.anchor);
            let names = directory_names(&bucket.file, MAX_ARCHIVE)?;
            for name in &names {
                if !is_record_name(name) || &name[..2] != prefix {
                    return Err(C10IntentStoreErrorV2::Corrupt);
                }
                let entry =
                    read_entry(&bucket.file, name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
                if !entry.history.is_terminal()
                    || record_name(entry.history.operation_id()) != *name
                    || !inventory.operations.insert(entry.history.operation_id())
                    || self
                        .store
                        .as_ref()
                        .map(|store| read_entry(&store.file, name))
                        .transpose()?
                        .flatten()
                        .is_some()
                {
                    return Err(C10IntentStoreErrorV2::Corrupt);
                }
                inventory.count += 1;
                inventory.bytes = add_bytes(inventory.bytes, &entry.bytes)?;
                if inventory.count > MAX_ARCHIVE || inventory.bytes > MAX_ARCHIVE_BYTES {
                    return Err(C10IntentStoreErrorV2::Blocked("archive capacity"));
                }
                inventory
                    .evidence
                    .insert(format!("{prefix}/{name}"), entry.evidence);
            }
            for name in &names {
                let expected = &inventory.evidence[&format!("{prefix}/{name}")];
                let entry =
                    read_entry(&bucket.file, name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
                if entry.evidence != *expected {
                    return Err(C10IntentStoreErrorV2::Blocked("archive readback changed"));
                }
            }
            if directory_names(&bucket.file, MAX_ARCHIVE)? != names {
                return Err(C10IntentStoreErrorV2::Blocked("archive bucket changed"));
            }
            verify_named(&archive.file, &component(prefix)?, bucket.anchor)?;
        }
        if directory_names(&archive.file, 256)? != buckets {
            return Err(C10IntentStoreErrorV2::Blocked("archive names changed"));
        }
        self.verify_tree()?;
        Ok(inventory)
    }

    #[cfg(unix)]
    fn initialize_for_publication(&mut self) -> Result<()> {
        self.lease
            .mutation()
            .map_err(|_| C10IntentStoreErrorV2::Blocked("exclusive project lease required"))?;
        self.verify_tree()?;
        self.require_empty_partial_layout()?;
        if self.sync.is_none() {
            fault_point(1)?;
            self.sync = Some(create_directory(&self.state.file, "sync")?);
            self.state.file.sync_all()?;
        }
        if self.store.is_none() {
            let sync = self.sync.as_ref().ok_or(C10IntentStoreErrorV2::Corrupt)?;
            fault_point(2)?;
            self.store = Some(create_directory(&sync.file, DIRECTORY)?);
            sync.file.sync_all()?;
        }
        let store = self.store.as_ref().ok_or(C10IntentStoreErrorV2::Corrupt)?;
        if self.archive.is_none() {
            fault_point(3)?;
            self.archive = Some(create_directory(&store.file, "archive")?);
            store.file.sync_all()?;
        }
        if self.owner_lock.is_none() {
            fault_point(4)?;
            let name = component("owner.lock")?;
            let lock = open_at(
                &store.file,
                &name,
                libc::O_RDONLY | libc::O_CREAT | libc::O_EXCL,
            )?;
            private_file(&lock.metadata()?)?;
            lock_exclusive(&lock)?;
            let anchor = Anchor::metadata(&lock.metadata()?);
            verify_named(&store.file, &name, anchor)?;
            lock.sync_all()?;
            store.file.sync_all()?;
            self.owner_lock = Some((lock, anchor));
        }
        self.verify_tree()?;
        Ok(())
    }

    #[cfg(all(test, unix))]
    fn test_fail_once_at(&self, point: u8) {
        FAULT_AT.with(|value| value.set(Some(point)));
    }
}

#[cfg(any(unix, test))]
fn active_terminal_pair(
    history: &SyncRemoteIntentHistoryV2,
    management: &ManagementEffectSnapshotV1,
    participation: &ParticipationSnapshotV2,
) -> Result<()> {
    let ParticipationReadV2::Current(part) = &participation.classification else {
        return Err(C10IntentStoreErrorV2::Blocked(
            "participation terminal pair",
        ));
    };
    durable_participation_pair(history, management, part, participation.exact_digest)
}
#[cfg(any(unix, test))]
fn durable_participation_pair(
    history: &SyncRemoteIntentHistoryV2,
    management: &ManagementEffectSnapshotV1,
    part: &super::super::participation::ParticipationV2,
    digest: Option<SyncDigestV1>,
) -> Result<()> {
    let identity = history.current().identity();
    let expected_part = terminal_management_pair(history, management)?;
    part.validate()
        .map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
    if digest != Some(expected_part)
        || part.inner().share.share_id != identity.share_id.as_str()
        || part.credential_binding != identity.account_binding
        || part
            .inner()
            .share
            .production_remote()
            .map_err(|_| C10IntentStoreErrorV2::Corrupt)?
            != identity.remote
    {
        return Err(C10IntentStoreErrorV2::Blocked(
            "participation terminal identity",
        ));
    }
    if let SyncRemoteIntentStateV2::Completed { receipt } = history.current().state()
        && part.inner().agreed_commit.as_deref() != Some(receipt.intended_commit.as_str())
    {
        return Err(C10IntentStoreErrorV2::Blocked(
            "participation agreed commit",
        ));
    }
    Ok(())
}

#[cfg(any(unix, test))]
fn terminal_management_pair(
    history: &SyncRemoteIntentHistoryV2,
    management: &ManagementEffectSnapshotV1,
) -> Result<SyncDigestV1> {
    let identity = history.current().identity();
    let request = &management.request;
    if request.operation_id != identity.operation_id
        || request.inventory.encoded() != "C10"
        || request.request_hash.as_hex() != identity.request_hash.as_hex()
        || request.scope.project.as_hex() != identity.project.as_hex()
        || request.scope.session.is_some()
        || request.scope.generation != identity.account_binding.generation()
        || request.scope.permission != ManagementPermissionScopeV1::Remote
        || request.scope.account_binding != Some(identity.management_account_binding_hash()?)
        || request.rewind != ManagementRewindDispositionV1::IrreversibleRemote
    {
        return Err(C10IntentStoreErrorV2::Blocked(
            "management terminal identity",
        ));
    }
    let terminal_hash = Hash::from_hex(&history.terminal_evidence_hash()?.as_hex())
        .map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
    match (history.current().state(), &management.state) {
        (
            SyncRemoteIntentStateV2::Completed { receipt },
            ManagementEffectStateV1::Completed { evidence_hash },
        ) if *evidence_hash == terminal_hash => Ok(receipt.participation_digest),
        (
            SyncRemoteIntentStateV2::ReconciledAbandon { .. },
            ManagementEffectStateV1::ReconciledAbandon { evidence_hash },
        ) if *evidence_hash == terminal_hash => Ok(identity.prior_participation_digest),
        _ => Err(C10IntentStoreErrorV2::Blocked("management terminal pair")),
    }
}

#[cfg(any(unix, test))]
enum NameKind {
    Record,
    Temporary { target: String, sequence: u64 },
}
#[cfg(any(unix, test))]
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
#[cfg(any(unix, test))]
fn is_record_name(value: &str) -> bool {
    value
        .strip_suffix(".json")
        .is_some_and(|hash| lower_hex(hash, 64))
}
#[cfg(any(unix, test))]
fn classify_name(name: &str) -> Result<NameKind> {
    if is_record_name(name) {
        return Ok(NameKind::Record);
    }
    let Some(rest) = name.strip_prefix('.') else {
        return Err(C10IntentStoreErrorV2::Corrupt);
    };
    let parts: Vec<_> = rest.split('.').collect();
    if parts.len() != 5
        || !lower_hex(parts[0], 64)
        || parts[1] != "json"
        || !lower_hex(parts[3], 24)
        || parts[4] != "tmp"
    {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    let sequence: u64 = parts[2]
        .parse()
        .map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
    if sequence == 0 || sequence.to_string() != parts[2] {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    Ok(NameKind::Temporary {
        target: format!("{}.json", parts[0]),
        sequence,
    })
}
#[cfg(any(unix, test))]
fn record_name(operation: Operation) -> String {
    format!(
        "{}.json",
        SyncDigestV1::digest(operation.as_hex().as_bytes()).as_hex()
    )
}
#[cfg(unix)]
fn temporary_name(target: &str, sequence: u64) -> Result<String> {
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("C10 temporary entropy"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(".{target}.{sequence}.{suffix}.tmp"))
}
#[cfg(any(unix, test))]
fn crate_component(name: &str) -> Result<CString> {
    component(name)
}
#[cfg(unix)]
fn read_entry(directory: &File, name: &str) -> Result<Option<Entry>> {
    let component = component(name)?;
    let file = match open_at(directory, &component, libc::O_RDONLY) {
        Ok(file) => file,
        Err(C10IntentStoreErrorV2::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            if named_stat(directory, &component)?.is_some() {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let bytes = bounded_bytes(&file, MAX_RECORD)?;
    let anchor = Anchor::metadata(&file.metadata()?);
    verify_named(directory, &component, anchor)?;
    let history = SyncRemoteIntentHistoryV2::from_canonical_bytes(&bytes)?;
    Ok(Some(Entry {
        history,
        evidence: EntryEvidence {
            anchor,
            length: bytes.len() as u64,
            hash: SyncDigestV1::digest(&bytes),
        },
        bytes,
        name: name.to_owned(),
    }))
}
#[cfg(any(unix, test))]
fn add_bytes(total: u64, bytes: &[u8]) -> Result<u64> {
    total
        .checked_add(bytes.len() as u64)
        .ok_or(C10IntentStoreErrorV2::Corrupt)
}
#[cfg(any(unix, test))]
fn capacity(counts: Counts) -> Result<()> {
    if counts.active > MAX_ACTIVE
        || counts.active_bytes > MAX_ACTIVE_BYTES
        || counts.temporary > MAX_TMP
        || counts.temporary_bytes > MAX_TMP_BYTES
        || counts.archive > MAX_ARCHIVE
        || counts.archive_bytes > MAX_ARCHIVE_BYTES
        || counts
            .active
            .checked_add(counts.temporary)
            .and_then(|n| n.checked_add(2))
            .is_none_or(|n| n > MAX_ROOT_ENTRIES)
    {
        return Err(C10IntentStoreErrorV2::Blocked("intent capacity"));
    }
    Ok(())
}

#[cfg(any(unix, test))]
fn component(name: &str) -> Result<CString, C10IntentStoreErrorV2> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    CString::new(name).map_err(|_| C10IntentStoreErrorV2::Corrupt)
}

#[cfg(unix)]
fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> Result<File, C10IntentStoreErrorV2> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_directory(
    parent: &File,
    name: &str,
) -> Result<Option<PinnedDirectory>, C10IntentStoreErrorV2> {
    let name = component(name)?;
    let file = match open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(file) => file,
        Err(C10IntentStoreErrorV2::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            if named_stat(parent, &name)?.is_some() {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    private_directory(&metadata)?;
    let anchor = Anchor::metadata(&metadata);
    verify_named(parent, &name, anchor)?;
    Ok(Some(PinnedDirectory { file, anchor }))
}

#[cfg(unix)]
fn create_directory(parent: &File, name: &str) -> Result<PinnedDirectory, C10IntentStoreErrorV2> {
    let name_component = component(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_component.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    open_directory(parent, name)?.ok_or(C10IntentStoreErrorV2::Corrupt)
}

#[cfg(unix)]
fn verify_optional_directory(
    parent: &File,
    name: &str,
    directory: Option<&PinnedDirectory>,
) -> Result<(), C10IntentStoreErrorV2> {
    let name = component(name)?;
    match directory {
        Some(directory) => {
            let metadata = directory.file.metadata()?;
            private_directory(&metadata)?;
            if Anchor::metadata(&metadata) != directory.anchor {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            verify_named(parent, &name, directory.anchor)
        }
        None if named_stat(parent, &name)?.is_some() => {
            Err(C10IntentStoreErrorV2::Blocked("intent directory appeared"))
        }
        None => Ok(()),
    }
}

#[cfg(unix)]
fn private_directory(metadata: &fs::Metadata) -> Result<(), C10IntentStoreErrorV2> {
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    Ok(())
}

#[cfg(unix)]
fn private_file(metadata: &fs::Metadata) -> Result<(), C10IntentStoreErrorV2> {
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    Ok(())
}

#[cfg(unix)]
fn named_stat(parent: &File, name: &CStr) -> Result<Option<libc::stat>, C10IntentStoreErrorV2> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(unsafe { stat.assume_init() }))
}

#[cfg(unix)]
fn verify_named(parent: &File, name: &CStr, expected: Anchor) -> Result<(), C10IntentStoreErrorV2> {
    let stat = named_stat(parent, name)?.ok_or(C10IntentStoreErrorV2::Corrupt)?;
    if Anchor::stat(&stat) != expected {
        return Err(C10IntentStoreErrorV2::Blocked(
            "intent named anchor changed",
        ));
    }
    if stat.st_mode as u32 & libc::S_IFMT as u32 == libc::S_IFREG as u32 && stat.st_nlink != 1 {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    Ok(())
}

#[cfg(unix)]
fn bounded_bytes(file: &File, maximum: u64) -> Result<Vec<u8>, C10IntentStoreErrorV2> {
    use std::os::unix::fs::FileExt;
    let before = file.metadata()?;
    private_file(&before)?;
    if before.len() > maximum {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    let mut bytes = vec![0; before.len() as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        let count = file.read_at(&mut bytes[offset..], offset as u64)?;
        if count == 0 {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        offset += count;
    }
    let mut extra = [0; 1];
    if file.read_at(&mut extra, bytes.len() as u64)? != 0 {
        return Err(C10IntentStoreErrorV2::Corrupt);
    }
    let after = file.metadata()?;
    private_file(&after)?;
    if Anchor::metadata(&before) != Anchor::metadata(&after)
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(C10IntentStoreErrorV2::Blocked(
            "intent file changed during inspection",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> Result<(), C10IntentStoreErrorV2> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(C10IntentStoreErrorV2::Blocked("intent owner is busy"));
        }
        return Err(error.into());
    }
    Ok(())
}

#[cfg(unix)]
fn rename_replace(
    from: &File,
    source: &CStr,
    to: &File,
    target: &CStr,
) -> Result<(), C10IntentStoreErrorV2> {
    if unsafe {
        libc::renameat(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn rename_no_replace(
    from: &File,
    source: &CStr,
    to: &File,
    target: &CStr,
) -> Result<(), C10IntentStoreErrorV2> {
    #[cfg(target_os = "linux")]
    let code = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let code = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(C10IntentStoreErrorV2::Unavailable(
        "atomic archive publication",
    ));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if code != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn directory_names(directory: &File, maximum: usize) -> Result<Vec<String>, C10IntentStoreErrorV2> {
    let fd = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
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
        let mut names = BTreeSet::new();
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
                .map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
            if name == "." || name == ".." {
                continue;
            }
            if !names.insert(name.to_owned()) || names.len() > maximum {
                return Err(C10IntentStoreErrorV2::Blocked(
                    "intent directory capacity or duplicate entry",
                ));
            }
        }
        Ok(names.into_iter().collect())
    })();
    let close_result = unsafe { libc::closedir(stream) };
    if close_result != 0 && result.is_ok() {
        return Err(io::Error::last_os_error().into());
    }
    result
}

#[cfg(all(test, unix))]
thread_local! {
    static FAULT_AT: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

#[cfg(unix)]
fn fault_point(point: u8) -> Result<(), C10IntentStoreErrorV2> {
    #[cfg(test)]
    if FAULT_AT.with(|value| {
        if value.get() == Some(point) {
            value.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(io::Error::other("C10 store test interruption").into());
    }
    let _ = point;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum C10LegacyStateV1 {
    BeforeSend,
    OutcomeUnknown,
    Completed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct C10LegacyDiagnosticV1 {
    pub operation_id: SyncRemoteOperationIdV1,
    pub state: C10LegacyStateV1,
    pub remote: super::super::share::SyncRemoteDisplayV1,
    pub manual_action_required: bool,
}
impl<'a> C10IntentStoreV2<'a> {
    /// A bounded redacted diagnosis; V1 records never reach the V2 write API.
    #[cfg(unix)]
    pub(crate) fn legacy_diagnostics(&self) -> Result<Vec<C10LegacyDiagnosticV1>> {
        self.verify_tree()?;
        let Some(sync) = &self.sync else {
            return Ok(Vec::new());
        };
        let Some(legacy) = open_directory(&sync.file, "remote-intents-v1")? else {
            return Ok(Vec::new());
        };
        let lock_name = component("owner.lock")?;
        let lock = open_at(&legacy.file, &lock_name, libc::O_RDONLY)?;
        if !bounded_bytes(&lock, 0)?.is_empty() {
            return Err(C10IntentStoreErrorV2::Corrupt);
        }
        let lock_anchor = Anchor::metadata(&lock.metadata()?);
        verify_named(&legacy.file, &lock_name, lock_anchor)?;
        lock_exclusive(&lock)?;
        let names = directory_names(&legacy.file, MAX_ACTIVE + 1)?;
        let mut diagnostics = Vec::new();
        let mut evidence = BTreeMap::new();
        let mut operations = BTreeSet::new();
        let mut total = 0_u64;
        for name in &names {
            if name == "owner.lock" {
                continue;
            }
            if !is_record_name(name) {
                return Err(C10IntentStoreErrorV2::Blocked(
                    "legacy pending or unknown entry",
                ));
            }
            let component = component(name)?;
            let file = open_at(&legacy.file, &component, libc::O_RDONLY)?;
            let bytes = bounded_bytes(&file, MAX_RECORD)?;
            total = add_bytes(total, &bytes)?;
            if total > MAX_ACTIVE_BYTES {
                return Err(C10IntentStoreErrorV2::Blocked("legacy capacity"));
            }
            let anchor = Anchor::metadata(&file.metadata()?);
            verify_named(&legacy.file, &component, anchor)?;
            let record: SyncRemoteIntentRecordV1 =
                serde_json::from_slice(&bytes).map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
            super::validate_record_invariants(&record)
                .map_err(|_| C10IntentStoreErrorV2::Corrupt)?;
            if format!(
                "{}.json",
                SyncDigestV1::digest(record.request.operation_id.as_str().as_bytes()).as_hex()
            ) != *name
                || !operations.insert(record.request.operation_id.clone())
            {
                return Err(C10IntentStoreErrorV2::Corrupt);
            }
            let state = match record.state {
                SyncRemoteIntentStateV1::BeforeSend => C10LegacyStateV1::BeforeSend,
                SyncRemoteIntentStateV1::OutcomeUnknown { .. } => C10LegacyStateV1::OutcomeUnknown,
                SyncRemoteIntentStateV1::Completed { .. } => C10LegacyStateV1::Completed,
            };
            let link = super::super::share::ShareLink {
                remote: record.request.canonical_remote.as_str().to_owned(),
                share_id: record.request.share_id.as_str().to_owned(),
                invitation_id: "legacy-diagnostic".into(),
                expires_unix_seconds: 1,
            };
            diagnostics.push(C10LegacyDiagnosticV1 {
                operation_id: record.request.operation_id,
                state,
                remote: link.remote_display(),
                manual_action_required: true,
            });
            evidence.insert(
                name.clone(),
                EntryEvidence {
                    anchor,
                    length: bytes.len() as u64,
                    hash: SyncDigestV1::digest(&bytes),
                },
            );
        }
        for (name, expected) in evidence {
            let component = component(&name)?;
            verify_named(&legacy.file, &component, expected.anchor)?;
            let bytes = bounded_bytes(
                &open_at(&legacy.file, &component, libc::O_RDONLY)?,
                MAX_RECORD,
            )?;
            if bytes.len() as u64 != expected.length
                || SyncDigestV1::digest(&bytes) != expected.hash
            {
                return Err(C10IntentStoreErrorV2::Blocked("legacy readback changed"));
            }
        }
        if directory_names(&legacy.file, MAX_ACTIVE + 1)? != names {
            return Err(C10IntentStoreErrorV2::Blocked("legacy names changed"));
        }
        verify_named(&legacy.file, &lock_name, lock_anchor)?;
        verify_named(&sync.file, &component("remote-intents-v1")?, legacy.anchor)?;
        self.verify_tree()?;
        Ok(diagnostics)
    }
}

#[cfg(test)]
mod data_tests;
#[cfg(all(test, unix))]
mod tests;

#[cfg(not(unix))]
fn unavailable() -> C10IntentStoreErrorV2 {
    C10IntentStoreErrorV2::Unavailable("sync intent storage is unavailable on this platform")
}

#[cfg(not(unix))]
impl<'a> C10IntentReadV2<'a> {
    pub(crate) fn inspect(
        _state: &Path,
        _lease: crate::project_mutation::ProjectReadLeaseV1<'a>,
    ) -> Result<Self> {
        Err(unavailable())
    }
}

#[cfg(not(unix))]
impl<'a> C10IntentStoreV2<'a> {
    pub(crate) fn inspect_at_state_dir(
        _state: &Path,
        _lease: &'a ProjectMutationLeaseV1,
    ) -> Result<Self> {
        Err(unavailable())
    }

    pub(crate) fn direct_lookup_at_state_dir(
        _state: &Path,
        _lease: &'a ProjectMutationLeaseV1,
        _operation: Operation,
    ) -> Result<Option<SyncRemoteIntentHistoryV2>> {
        Err(unavailable())
    }

    pub(crate) fn archive_page_at_state_dir(
        _state: &Path,
        _lease: &'a ProjectMutationLeaseV1,
        _cursor: Option<&C10ArchiveCursorV2>,
    ) -> Result<C10ArchivePageV2> {
        Err(unavailable())
    }

    pub(crate) fn pending_history_for_operation(
        &self,
        _operation: Operation,
    ) -> Result<Option<SyncRemoteIntentHistoryV2>> {
        Err(unavailable())
    }

    pub(crate) fn lookup(
        &self,
        _operation: Operation,
    ) -> Result<Option<SyncRemoteIntentHistoryV2>> {
        Err(unavailable())
    }

    pub(crate) fn archived_terminal_pair_readback(
        &self,
        _operation: Operation,
        _management: &ManagementEffectSnapshotV1,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        Err(unavailable())
    }

    pub(crate) fn active_terminal_pair_readback(
        &self,
        _operation: Operation,
        _management: &ManagementEffectSnapshotV1,
        _participation: &ParticipationSnapshotV2,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        Err(unavailable())
    }

    pub(crate) fn archive_inventory_digest(&self) -> Result<SyncDigestV1> {
        Err(unavailable())
    }

    pub(crate) fn verify_read_inventory(&self, _archive: SyncDigestV1) -> Result<()> {
        Err(unavailable())
    }

    pub(crate) fn active_durable_terminal_pair_readback(
        &self,
        _operation: Operation,
        _management: &ManagementEffectSnapshotV1,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        Err(unavailable())
    }

    pub(crate) fn complete_initial_layout(
        &mut self,
        _reservation: &mut C10IntentReservationV2,
    ) -> Result<()> {
        Err(unavailable())
    }

    pub(crate) fn reserve_operation(
        &mut self,
        _operation: Operation,
    ) -> Result<C10IntentReservationV2> {
        Err(unavailable())
    }

    pub(crate) fn publish(
        &mut self,
        _reservation: &mut C10IntentReservationV2,
        _history: &SyncRemoteIntentHistoryV2,
    ) -> Result<()> {
        Err(unavailable())
    }

    pub(crate) fn resume_pending(
        &mut self,
        _operation: Operation,
    ) -> Result<SyncRemoteIntentHistoryV2> {
        Err(unavailable())
    }

    pub(crate) fn terminal_readback(
        &mut self,
        _reservation: &C10IntentReservationV2,
        _management: &ManagementEffectSnapshotV1,
        _participation: &ParticipationSnapshotV2,
    ) -> Result<C10TerminalReadbackV2> {
        Err(unavailable())
    }

    pub(crate) fn archive_terminal(
        &mut self,
        _reservation: &mut C10IntentReservationV2,
        _completion: C10TerminalReadbackV2,
    ) -> Result<()> {
        Err(unavailable())
    }

    pub(crate) fn archive_page(
        &self,
        _cursor: Option<&C10ArchiveCursorV2>,
    ) -> Result<C10ArchivePageV2> {
        Err(unavailable())
    }

    pub(crate) fn legacy_diagnostics(&self) -> Result<Vec<C10LegacyDiagnosticV1>> {
        Err(unavailable())
    }
}
