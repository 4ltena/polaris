//! Strict descriptor-relative storage for C08 history and no-intent evidence.
//!
//! Inspection and reservation never create files. The parent owner may publish
//! only after its management effect is durably Started.

use super::{C08StoredRecordV1, CredentialRebindErrorV1};
use crate::management_effect::{ManagementEffectHashV1, ManagementOperationIdV1};
use serde::Serialize;
#[cfg(any(unix, test))]
use std::str::FromStr;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    collections::BTreeSet,
    ffi::CStr,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
};

#[cfg(unix)]
const DIRECTORY: &str = "credential-rebind-v1";
#[cfg(any(unix, test))]
const MAX_RECORD: u64 = 64 * 1024;
#[cfg(any(unix, test))]
const MAX_ACTIVE: usize = 64;
#[cfg(any(unix, test))]
const MAX_ACTIVE_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_TMP: usize = 16;
#[cfg(any(unix, test))]
const MAX_TMP_BYTES: u64 = 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_ARCHIVE: usize = 1024;
#[cfg(any(unix, test))]
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct Anchor {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
}

#[cfg(unix)]
impl Anchor {
    fn metadata(value: &fs::Metadata) -> Self {
        Self {
            device: value.dev(),
            inode: value.ino(),
            mode: value.mode(),
            uid: value.uid(),
        }
    }

    fn stat(value: &libc::stat) -> Self {
        Self {
            device: value.st_dev as u64,
            #[cfg(target_pointer_width = "64")]
            inode: value.st_ino,
            #[cfg(not(target_pointer_width = "64"))]
            inode: value.st_ino as u64,
            mode: value.st_mode as u32,
            uid: value.st_uid,
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
    length: Option<u64>,
    hash: Option<ManagementEffectHashV1>,
}

impl EntryEvidence {
    #[cfg(any(unix, test))]
    fn directory(anchor: Anchor) -> Self {
        Self {
            anchor,
            length: None,
            hash: None,
        }
    }

    #[cfg(any(unix, test))]
    fn file(anchor: Anchor, bytes: &[u8]) -> Self {
        Self {
            anchor,
            length: Some(bytes.len() as u64),
            hash: Some(ManagementEffectHashV1::digest(bytes)),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Location {
    Active,
    Archive,
}

struct StoredEntry {
    record: C08StoredRecordV1,
    bytes: Vec<u8>,
    name: String,
    location: Location,
}

#[derive(Default)]
struct Counts {
    active: usize,
    active_bytes: u64,
    temporary: usize,
    temporary_bytes: u64,
    archive: usize,
    archive_bytes: u64,
}

struct Inventory {
    evidence: BTreeMap<String, EntryEvidence>,
    records: BTreeMap<ManagementOperationIdV1, StoredEntry>,
    counts: Counts,
}

impl Inventory {
    fn digest(&self) -> ManagementEffectHashV1 {
        // Only canonical names, physical anchors and hashes of nonsecret C08
        // records are included. Record payloads are never written to a log.
        let bytes = serde_json::to_vec(&("polaris-c08-intent-inventory-v1", &self.evidence))
            .expect("bounded inventory contains only serializable primitive values");
        ManagementEffectHashV1::digest(&bytes)
    }
}

pub(super) struct C08IntentStoreV1 {
    state_path: PathBuf,
    parent_path: PathBuf,
    state_name: CString,
    parent: PinnedDirectory,
    state: PinnedDirectory,
    sync: Option<PinnedDirectory>,
    store: Option<PinnedDirectory>,
    archive: Option<PinnedDirectory>,
    owner_lock: Option<(File, Anchor)>,
    inventory: Inventory,
    reserved: Option<ManagementOperationIdV1>,
}

impl C08IntentStoreV1 {
    #[cfg(unix)]
    pub(super) fn inspect(state: &Path) -> Result<Self, CredentialRebindErrorV1> {
        if !state.is_absolute() || fs::canonicalize(state)? != state {
            return Err(CredentialRebindErrorV1::Blocked(
                "noncanonical state directory",
            ));
        }
        let parent_path = state
            .parent()
            .ok_or(CredentialRebindErrorV1::Corrupt)?
            .to_owned();
        let state_name = CString::new(
            state
                .file_name()
                .ok_or(CredentialRebindErrorV1::Corrupt)?
                .as_bytes(),
        )
        .map_err(|_| CredentialRebindErrorV1::Corrupt)?;
        let parent_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&parent_path)?;
        let parent_meta = parent_file.metadata()?;
        if !parent_meta.is_dir() {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        let parent = PinnedDirectory {
            file: parent_file,
            anchor: Anchor::metadata(&parent_meta),
        };
        let state_file = open_at(
            &parent.file,
            &state_name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?;
        let state_meta = state_file.metadata()?;
        private_directory(&state_meta)?;
        let state = PinnedDirectory {
            file: state_file,
            anchor: Anchor::metadata(&state_meta),
        };
        verify_named(&parent.file, &state_name, state.anchor)?;
        let sync = open_directory(&state.file, "sync")?;
        let store = match &sync {
            Some(sync) => open_directory(&sync.file, DIRECTORY)?,
            None => None,
        };
        let (archive, owner_lock) = match &store {
            Some(store) => {
                let archive = open_directory(&store.file, "archive")?
                    .ok_or(CredentialRebindErrorV1::Corrupt)?;
                let name = component("owner.lock")?;
                let lock = open_at(&store.file, &name, libc::O_RDONLY)?;
                let metadata = lock.metadata()?;
                private_file(&metadata)?;
                if metadata.len() != 0 {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
                let anchor = Anchor::metadata(&metadata);
                verify_named(&store.file, &name, anchor)?;
                lock_exclusive(&lock)?;
                verify_named(&store.file, &name, anchor)?;
                (Some(archive), Some((lock, anchor)))
            }
            None => (None, None),
        };
        let mut result = Self {
            state_path: state_path(&parent_path, &state_name),
            parent_path,
            state_name,
            parent,
            state,
            sync,
            store,
            archive,
            owner_lock,
            inventory: Inventory {
                evidence: BTreeMap::new(),
                records: BTreeMap::new(),
                counts: Counts::default(),
            },
            reserved: None,
        };
        result.inventory = result.scan(false)?;
        Ok(result)
    }

    #[cfg(all(test, unix))]
    pub(super) fn test_fail_once_at(&self, point: u8) {
        FAULT_AT.with(|value| value.set(Some(point)));
    }

    pub(super) fn lookup(&self, id: ManagementOperationIdV1) -> Option<&C08StoredRecordV1> {
        self.inventory.records.get(&id).map(|entry| &entry.record)
    }

    pub(super) fn records(&self) -> impl Iterator<Item = &C08StoredRecordV1> {
        self.inventory.records.values().map(|entry| &entry.record)
    }

    pub(super) fn inventory_digest(&self) -> ManagementEffectHashV1 {
        self.inventory.digest()
    }

    pub(super) fn is_archived(&self, id: ManagementOperationIdV1) -> bool {
        self.inventory
            .records
            .get(&id)
            .is_some_and(|entry| entry.location == Location::Archive)
    }

    pub(super) fn operation_absent(&self, id: ManagementOperationIdV1) -> bool {
        !self.inventory.records.contains_key(&id)
    }

    /// Re-read through the existing descriptors and lock. Changes by another
    /// writer or a replaced named entry are blockers, rather than new evidence.
    #[cfg(unix)]
    pub(super) fn refresh(&mut self) -> Result<(), CredentialRebindErrorV1> {
        let fresh = self.scan(false)?;
        if fresh.evidence != self.inventory.evidence {
            return Err(CredentialRebindErrorV1::Blocked("intent inventory changed"));
        }
        self.inventory = fresh;
        Ok(())
    }

    /// Read-only reservation of the worst-case complete record in all three
    /// capacity classes. Existing active records reserve growth, not a second ID.
    #[cfg(unix)]
    pub(super) fn reserve_operation(
        &mut self,
        id: ManagementOperationIdV1,
    ) -> Result<(), CredentialRebindErrorV1> {
        self.refresh()?;
        let existing = self.inventory.records.get(&id);
        if existing.is_some_and(|entry| entry.location == Location::Archive) {
            return Err(CredentialRebindErrorV1::Blocked(
                "archived operation is immutable",
            ));
        }
        let counts = &self.inventory.counts;
        let replaced_bytes = existing.map_or(0, |entry| entry.bytes.len() as u64);
        capacity(
            counts.active + usize::from(existing.is_none()),
            counts
                .active_bytes
                .checked_sub(replaced_bytes)
                .and_then(|n| n.checked_add(MAX_RECORD))
                .ok_or(CredentialRebindErrorV1::Corrupt)?,
            counts.temporary + 1,
            counts
                .temporary_bytes
                .checked_add(MAX_RECORD)
                .ok_or(CredentialRebindErrorV1::Corrupt)?,
            counts.archive + 1,
            counts
                .archive_bytes
                .checked_add(MAX_RECORD)
                .ok_or(CredentialRebindErrorV1::Corrupt)?,
        )?;
        self.reserved = Some(id);
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn publish(
        &mut self,
        record: &C08StoredRecordV1,
    ) -> Result<(), CredentialRebindErrorV1> {
        let bytes = record.canonical_bytes()?;
        let id = record.operation_id();
        if bytes.len() as u64 > MAX_RECORD || self.reserved != Some(id) {
            return Err(CredentialRebindErrorV1::Blocked(
                "intent operation is not reserved",
            ));
        }
        self.refresh()?;
        if let Some(prior) = self.inventory.records.get(&id) {
            if prior.location != Location::Active {
                return Err(CredentialRebindErrorV1::Blocked(
                    "archived operation is immutable",
                ));
            }
            record.is_extension_of(&prior.record)?;
            if bytes == prior.bytes {
                return Ok(());
            }
        } else if let C08StoredRecordV1::IntentHistory { records } = record
            && (records.len() != 1 || records[0].state != super::C08IntentStateV1::BeforeWrite)
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        // Initialization is the first filesystem mutation, and is called only
        // after the parent owner has written EffectStarted.
        self.initialize_for_publication()?;
        self.refresh()?;
        let directory = &self
            .store
            .as_ref()
            .ok_or(CredentialRebindErrorV1::Corrupt)?
            .file;
        let target = record_name(record);
        if let Some(prior) = self.inventory.records.get(&id)
            && prior.name != target
        {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        let temporary_name = format!("{}.tmp", id.as_hex());
        let temporary_component = component(&temporary_name)?;
        fault_point(13)?;
        let mut temporary = open_at(
            directory,
            &temporary_component,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )?;
        let metadata = temporary.metadata()?;
        private_file(&metadata)?;
        let temporary_anchor = Anchor::metadata(&metadata);
        verify_named(directory, &temporary_component, temporary_anchor)?;
        fault_point(5)?;
        temporary.write_all(&bytes)?;
        fault_point(6)?;
        temporary.sync_all()?;
        fault_point(7)?;
        self.verify_tree()?;
        verify_named(directory, &temporary_component, temporary_anchor)?;
        let mut before_rename = self.scan(true)?;
        let temporary_key = format!("active/{temporary_name}");
        let temporary_evidence = before_rename
            .evidence
            .remove(&temporary_key)
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        if temporary_evidence != EntryEvidence::file(temporary_anchor, &bytes)
            || before_rename.counts.temporary != 1
            || before_rename.evidence != self.inventory.evidence
        {
            return Err(CredentialRebindErrorV1::Blocked("intent inventory changed"));
        }
        let target_component = component(&target)?;
        if self.inventory.records.contains_key(&id) {
            let prior_evidence = self
                .inventory
                .evidence
                .get(&format!("active/{target}"))
                .ok_or(CredentialRebindErrorV1::Corrupt)?;
            verify_named(directory, &target_component, prior_evidence.anchor)?;
            rename_replace(
                directory,
                &temporary_component,
                directory,
                &target_component,
            )?;
        } else {
            rename_no_replace(
                directory,
                &temporary_component,
                directory,
                &target_component,
            )?;
        }
        fault_point(8)?;
        directory.sync_all()?;
        fault_point(9)?;
        self.verify_tree()?;
        verify_named(directory, &target_component, temporary_anchor)?;
        let readback = open_at(directory, &target_component, libc::O_RDONLY)?;
        if bounded_bytes(&readback, MAX_RECORD)? != bytes {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        verify_named(directory, &target_component, temporary_anchor)?;
        let mut expected = self.inventory.evidence.clone();
        expected.insert(format!("active/{target}"), temporary_evidence);
        let published = self.scan(false)?;
        if published.evidence != expected
            || published
                .records
                .get(&id)
                .is_none_or(|entry| entry.bytes != bytes)
        {
            return Err(CredentialRebindErrorV1::Blocked(
                "intent publication readback changed",
            ));
        }
        self.inventory = published;
        Ok(())
    }

    /// The parent calls this only after management terminal readback. It is a
    /// durable move, never garbage collection or replacement of an archive.
    #[cfg(unix)]
    pub(super) fn archive_terminal(
        &mut self,
        id: ManagementOperationIdV1,
    ) -> Result<(), CredentialRebindErrorV1> {
        self.refresh()?;
        let entry = self
            .inventory
            .records
            .get(&id)
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        if !entry.record.is_terminal() {
            return Err(CredentialRebindErrorV1::Blocked(
                "nonterminal intent cannot be archived",
            ));
        }
        if entry.location == Location::Archive {
            return Ok(());
        }
        if self.reserved != Some(id) {
            return Err(CredentialRebindErrorV1::Blocked(
                "intent operation is not reserved",
            ));
        }
        let counts = &self.inventory.counts;
        if counts.archive + 1 > MAX_ARCHIVE
            || counts
                .archive_bytes
                .checked_add(entry.bytes.len() as u64)
                .is_none_or(|n| n > MAX_ARCHIVE_BYTES)
        {
            return Err(CredentialRebindErrorV1::Blocked("intent archive capacity"));
        }
        let source = self
            .store
            .as_ref()
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        let archive = self
            .archive
            .as_ref()
            .ok_or(CredentialRebindErrorV1::Corrupt)?;
        let name = component(&entry.name)?;
        let active_key = format!("active/{}", entry.name);
        let archived_key = format!("archive/{}", entry.name);
        let evidence = self
            .inventory
            .evidence
            .get(&active_key)
            .ok_or(CredentialRebindErrorV1::Corrupt)?
            .clone();
        verify_named(&source.file, &name, evidence.anchor)?;
        rename_no_replace(&source.file, &name, &archive.file, &name)?;
        fault_point(10)?;
        archive.file.sync_all()?;
        fault_point(11)?;
        source.file.sync_all()?;
        fault_point(12)?;
        self.verify_tree()?;
        verify_named(&archive.file, &name, evidence.anchor)?;
        let readback = open_at(&archive.file, &name, libc::O_RDONLY)?;
        if bounded_bytes(&readback, MAX_RECORD)? != entry.bytes {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        let mut expected = self.inventory.evidence.clone();
        expected.remove(&active_key);
        expected.insert(archived_key, evidence);
        let archived = self.scan(false)?;
        if archived.evidence != expected {
            return Err(CredentialRebindErrorV1::Blocked(
                "intent archive readback changed",
            ));
        }
        self.inventory = archived;
        Ok(())
    }

    #[cfg(unix)]
    fn verify_tree(&self) -> Result<(), CredentialRebindErrorV1> {
        let parent_meta = fs::symlink_metadata(&self.parent_path)?;
        if !parent_meta.is_dir()
            || Anchor::metadata(&parent_meta) != self.parent.anchor
            || fs::canonicalize(&self.parent_path)? != self.parent_path
            || Anchor::metadata(&self.parent.file.metadata()?) != self.parent.anchor
        {
            return Err(CredentialRebindErrorV1::Blocked("state parent changed"));
        }
        let state_meta = self.state.file.metadata()?;
        private_directory(&state_meta)?;
        if Anchor::metadata(&state_meta) != self.state.anchor {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        verify_named(&self.parent.file, &self.state_name, self.state.anchor)?;
        if fs::canonicalize(&self.state_path)? != self.state_path {
            return Err(CredentialRebindErrorV1::Blocked("state directory changed"));
        }
        verify_optional_directory(&self.state.file, "sync", self.sync.as_ref())?;
        match &self.sync {
            Some(sync) => verify_optional_directory(&sync.file, DIRECTORY, self.store.as_ref())?,
            None if self.store.is_some() => return Err(CredentialRebindErrorV1::Corrupt),
            None => {}
        }
        match &self.store {
            Some(store) => {
                verify_optional_directory(&store.file, "archive", self.archive.as_ref())?;
                let (lock, anchor) = self
                    .owner_lock
                    .as_ref()
                    .ok_or(CredentialRebindErrorV1::Corrupt)?;
                let metadata = lock.metadata()?;
                private_file(&metadata)?;
                if metadata.len() != 0 || Anchor::metadata(&metadata) != *anchor {
                    return Err(CredentialRebindErrorV1::Corrupt);
                }
                verify_named(&store.file, &component("owner.lock")?, *anchor)?;
            }
            None if self.archive.is_some() || self.owner_lock.is_some() => {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            None => {}
        }
        Ok(())
    }

    #[cfg(unix)]
    fn scan(&self, allow_temporary: bool) -> Result<Inventory, CredentialRebindErrorV1> {
        self.verify_tree()?;
        let mut inventory = Inventory {
            evidence: BTreeMap::from([
                (
                    "state-parent".to_owned(),
                    EntryEvidence::directory(self.parent.anchor),
                ),
                (
                    "state".to_owned(),
                    EntryEvidence::directory(self.state.anchor),
                ),
            ]),
            records: BTreeMap::new(),
            counts: Counts::default(),
        };
        if let Some(sync) = &self.sync {
            inventory
                .evidence
                .insert("sync".to_owned(), EntryEvidence::directory(sync.anchor));
        }
        if let Some(store) = &self.store {
            inventory
                .evidence
                .insert("store".to_owned(), EntryEvidence::directory(store.anchor));
            let archive = self
                .archive
                .as_ref()
                .ok_or(CredentialRebindErrorV1::Corrupt)?;
            inventory.evidence.insert(
                "archive".to_owned(),
                EntryEvidence::directory(archive.anchor),
            );
            let (lock, anchor) = self
                .owner_lock
                .as_ref()
                .ok_or(CredentialRebindErrorV1::Corrupt)?;
            if !bounded_bytes(lock, 0)?.is_empty() {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            inventory
                .evidence
                .insert("owner.lock".to_owned(), EntryEvidence::file(*anchor, &[]));
            self.scan_directory(&store.file, Location::Active, &mut inventory)?;
            self.scan_directory(&archive.file, Location::Archive, &mut inventory)?;
        }
        self.verify_tree()?;
        let counts = &inventory.counts;
        capacity(
            counts.active,
            counts.active_bytes,
            counts.temporary,
            counts.temporary_bytes,
            counts.archive,
            counts.archive_bytes,
        )?;
        self.verify_inventory_evidence(&inventory)?;
        self.verify_tree()?;
        if !allow_temporary && counts.temporary != 0 {
            return Err(CredentialRebindErrorV1::Blocked(
                "unfinished intent temporary file",
            ));
        }
        Ok(inventory)
    }

    #[cfg(unix)]
    fn verify_inventory_evidence(
        &self,
        inventory: &Inventory,
    ) -> Result<(), CredentialRebindErrorV1> {
        for (key, expected) in &inventory.evidence {
            let Some(length) = expected.length else {
                continue;
            };
            let (directory, name, maximum) = if key == "owner.lock" {
                (
                    &self
                        .store
                        .as_ref()
                        .ok_or(CredentialRebindErrorV1::Corrupt)?
                        .file,
                    "owner.lock",
                    0,
                )
            } else if let Some(name) = key.strip_prefix("active/") {
                (
                    &self
                        .store
                        .as_ref()
                        .ok_or(CredentialRebindErrorV1::Corrupt)?
                        .file,
                    name,
                    MAX_RECORD,
                )
            } else if let Some(name) = key.strip_prefix("archive/") {
                (
                    &self
                        .archive
                        .as_ref()
                        .ok_or(CredentialRebindErrorV1::Corrupt)?
                        .file,
                    name,
                    MAX_RECORD,
                )
            } else {
                return Err(CredentialRebindErrorV1::Corrupt);
            };
            let name = component(name)?;
            verify_named(directory, &name, expected.anchor)?;
            let file = open_at(directory, &name, libc::O_RDONLY)?;
            if Anchor::metadata(&file.metadata()?) != expected.anchor {
                return Err(CredentialRebindErrorV1::Blocked(
                    "intent file anchor changed",
                ));
            }
            let bytes = bounded_bytes(&file, maximum)?;
            if bytes.len() as u64 != length
                || Some(ManagementEffectHashV1::digest(&bytes)) != expected.hash
            {
                return Err(CredentialRebindErrorV1::Blocked(
                    "intent inventory changed during inspection",
                ));
            }
            verify_named(directory, &name, expected.anchor)?;
        }
        for (prefix, directory, limit) in [
            ("active/", self.store.as_ref(), MAX_ACTIVE + MAX_TMP + 2),
            ("archive/", self.archive.as_ref(), MAX_ARCHIVE),
        ] {
            if let Some(directory) = directory {
                let mut expected: Vec<_> = inventory
                    .evidence
                    .keys()
                    .filter_map(|key| key.strip_prefix(prefix).map(str::to_owned))
                    .collect();
                if prefix == "active/" {
                    expected.extend(["archive".to_owned(), "owner.lock".to_owned()]);
                }
                expected.sort();
                if directory_names(&directory.file, limit)? != expected {
                    return Err(CredentialRebindErrorV1::Blocked(
                        "intent directory changed during inspection",
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn scan_directory(
        &self,
        directory: &File,
        location: Location,
        inventory: &mut Inventory,
    ) -> Result<(), CredentialRebindErrorV1> {
        let limit = match location {
            Location::Active => MAX_ACTIVE + MAX_TMP + 2,
            Location::Archive => MAX_ARCHIVE,
        };
        let names = directory_names(directory, limit)?;
        for name in &names {
            if location == Location::Active && (name == "owner.lock" || name == "archive") {
                continue;
            }
            let (id, kind) = classify_name(name)?;
            if location == Location::Archive && kind == EntryKind::Temporary {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            let component = component(name)?;
            let file = open_at(directory, &component, libc::O_RDONLY)?;
            let metadata = file.metadata()?;
            private_file(&metadata)?;
            let anchor = Anchor::metadata(&metadata);
            verify_named(directory, &component, anchor)?;
            let bytes = bounded_bytes(&file, MAX_RECORD)?;
            verify_named(directory, &component, anchor)?;
            let prefix = match location {
                Location::Active => "active",
                Location::Archive => "archive",
            };
            inventory.evidence.insert(
                format!("{prefix}/{name}"),
                EntryEvidence::file(anchor, &bytes),
            );
            if kind == EntryKind::Temporary {
                inventory.counts.temporary += 1;
                inventory.counts.temporary_bytes =
                    add_bytes(inventory.counts.temporary_bytes, &bytes)?;
                continue;
            }
            let record: C08StoredRecordV1 =
                serde_json::from_slice(&bytes).map_err(|_| CredentialRebindErrorV1::Corrupt)?;
            let canonical = record.canonical_bytes()?;
            if record.operation_id() != id
                || canonical != bytes
                || (kind == EntryKind::NoIntent)
                    != matches!(record, C08StoredRecordV1::NoIntentAbandon { .. })
                || (location == Location::Archive && !record.is_terminal())
            {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            match location {
                Location::Active => {
                    inventory.counts.active += 1;
                    inventory.counts.active_bytes =
                        add_bytes(inventory.counts.active_bytes, &bytes)?;
                }
                Location::Archive => {
                    inventory.counts.archive += 1;
                    inventory.counts.archive_bytes =
                        add_bytes(inventory.counts.archive_bytes, &bytes)?;
                }
            }
            if inventory
                .records
                .insert(
                    id,
                    StoredEntry {
                        record,
                        bytes,
                        name: name.clone(),
                        location,
                    },
                )
                .is_some()
            {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
        }
        if directory_names(directory, limit)? != names {
            return Err(CredentialRebindErrorV1::Blocked(
                "intent directory changed during inspection",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn initialize_for_publication(&mut self) -> Result<(), CredentialRebindErrorV1> {
        if self.store.is_some() {
            return Ok(());
        }
        self.refresh()?;
        if self.sync.is_none() {
            let sync = create_directory(&self.state.file, "sync")?;
            fault_point(1)?;
            self.state.file.sync_all()?;
            self.sync = Some(sync);
        }
        let sync = self.sync.as_ref().ok_or(CredentialRebindErrorV1::Corrupt)?;
        let store = create_directory(&sync.file, DIRECTORY)?;
        fault_point(2)?;
        sync.file.sync_all()?;
        let name = component("owner.lock")?;
        let lock = open_at(
            &store.file,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )?;
        let metadata = lock.metadata()?;
        private_file(&metadata)?;
        let anchor = Anchor::metadata(&metadata);
        verify_named(&store.file, &name, anchor)?;
        fault_point(3)?;
        lock.sync_all()?;
        lock_exclusive(&lock)?;
        let archive = create_directory(&store.file, "archive")?;
        fault_point(4)?;
        archive.file.sync_all()?;
        store.file.sync_all()?;
        self.owner_lock = Some((lock, anchor));
        self.archive = Some(archive);
        self.store = Some(store);
        self.inventory = self.scan(false)?;
        Ok(())
    }
}

#[cfg(any(unix, test))]
#[derive(Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Intent,
    NoIntent,
    Temporary,
}

#[cfg(any(unix, test))]
fn record_name(record: &C08StoredRecordV1) -> String {
    let suffix = if matches!(record, C08StoredRecordV1::NoIntentAbandon { .. }) {
        ".no-intent.json"
    } else {
        ".json"
    };
    format!("{}{suffix}", record.operation_id().as_hex())
}

#[cfg(any(unix, test))]
fn classify_name(
    name: &str,
) -> Result<(ManagementOperationIdV1, EntryKind), CredentialRebindErrorV1> {
    let (id, kind) = if let Some(id) = name.strip_suffix(".no-intent.json") {
        (id, EntryKind::NoIntent)
    } else if let Some(id) = name.strip_suffix(".json") {
        (id, EntryKind::Intent)
    } else if let Some(id) = name.strip_suffix(".tmp") {
        (id, EntryKind::Temporary)
    } else {
        return Err(CredentialRebindErrorV1::Corrupt);
    };
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    Ok((
        ManagementOperationIdV1::from_str(id).map_err(|_| CredentialRebindErrorV1::Corrupt)?,
        kind,
    ))
}

#[cfg(unix)]
fn state_path(parent: &Path, name: &CStr) -> PathBuf {
    use std::ffi::OsStr;
    parent.join(OsStr::from_bytes(name.to_bytes()))
}

#[cfg(any(unix, test))]
fn add_bytes(total: u64, bytes: &[u8]) -> Result<u64, CredentialRebindErrorV1> {
    total
        .checked_add(bytes.len() as u64)
        .ok_or(CredentialRebindErrorV1::Corrupt)
}

#[cfg(any(unix, test))]
fn capacity(
    active: usize,
    active_bytes: u64,
    temporary: usize,
    temporary_bytes: u64,
    archive: usize,
    archive_bytes: u64,
) -> Result<(), CredentialRebindErrorV1> {
    if active > MAX_ACTIVE
        || active_bytes > MAX_ACTIVE_BYTES
        || temporary > MAX_TMP
        || temporary_bytes > MAX_TMP_BYTES
        || archive > MAX_ARCHIVE
        || archive_bytes > MAX_ARCHIVE_BYTES
    {
        return Err(CredentialRebindErrorV1::Blocked("intent store capacity"));
    }
    Ok(())
}

#[cfg(any(unix, test))]
fn component(name: &str) -> Result<CString, CredentialRebindErrorV1> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    CString::new(name).map_err(|_| CredentialRebindErrorV1::Corrupt)
}

#[cfg(unix)]
fn open_at(
    parent: &File,
    name: &CStr,
    flags: libc::c_int,
) -> Result<File, CredentialRebindErrorV1> {
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
) -> Result<Option<PinnedDirectory>, CredentialRebindErrorV1> {
    let name = component(name)?;
    let file = match open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(file) => file,
        Err(CredentialRebindErrorV1::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            if named_stat(parent, &name)?.is_some() {
                return Err(CredentialRebindErrorV1::Corrupt);
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
fn create_directory(parent: &File, name: &str) -> Result<PinnedDirectory, CredentialRebindErrorV1> {
    let name_component = component(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_component.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    open_directory(parent, name)?.ok_or(CredentialRebindErrorV1::Corrupt)
}

#[cfg(unix)]
fn verify_optional_directory(
    parent: &File,
    name: &str,
    directory: Option<&PinnedDirectory>,
) -> Result<(), CredentialRebindErrorV1> {
    let name = component(name)?;
    match directory {
        Some(directory) => {
            let metadata = directory.file.metadata()?;
            private_directory(&metadata)?;
            if Anchor::metadata(&metadata) != directory.anchor {
                return Err(CredentialRebindErrorV1::Corrupt);
            }
            verify_named(parent, &name, directory.anchor)
        }
        None if named_stat(parent, &name)?.is_some() => Err(CredentialRebindErrorV1::Blocked(
            "intent directory appeared",
        )),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn private_directory(metadata: &fs::Metadata) -> Result<(), CredentialRebindErrorV1> {
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    Ok(())
}

#[cfg(unix)]
fn private_file(metadata: &fs::Metadata) -> Result<(), CredentialRebindErrorV1> {
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    Ok(())
}

#[cfg(unix)]
fn named_stat(parent: &File, name: &CStr) -> Result<Option<libc::stat>, CredentialRebindErrorV1> {
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
fn verify_named(
    parent: &File,
    name: &CStr,
    expected: Anchor,
) -> Result<(), CredentialRebindErrorV1> {
    let stat = named_stat(parent, name)?.ok_or(CredentialRebindErrorV1::Corrupt)?;
    if Anchor::stat(&stat) != expected {
        return Err(CredentialRebindErrorV1::Blocked(
            "intent named anchor changed",
        ));
    }
    if stat.st_mode as u32 & libc::S_IFMT as u32 == libc::S_IFREG as u32 && stat.st_nlink != 1 {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    Ok(())
}

#[cfg(unix)]
fn bounded_bytes(file: &File, maximum: u64) -> Result<Vec<u8>, CredentialRebindErrorV1> {
    use std::os::unix::fs::FileExt;
    let before = file.metadata()?;
    private_file(&before)?;
    if before.len() > maximum {
        return Err(CredentialRebindErrorV1::Corrupt);
    }
    let mut bytes = vec![0; before.len() as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        let count = file.read_at(&mut bytes[offset..], offset as u64)?;
        if count == 0 {
            return Err(CredentialRebindErrorV1::Corrupt);
        }
        offset += count;
    }
    let mut extra = [0; 1];
    if file.read_at(&mut extra, bytes.len() as u64)? != 0 {
        return Err(CredentialRebindErrorV1::Corrupt);
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
        return Err(CredentialRebindErrorV1::Blocked(
            "intent file changed during inspection",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> Result<(), CredentialRebindErrorV1> {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(CredentialRebindErrorV1::Blocked("intent owner is busy"));
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
) -> Result<(), CredentialRebindErrorV1> {
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
) -> Result<(), CredentialRebindErrorV1> {
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
    return Err(CredentialRebindErrorV1::Unavailable(
        "atomic archive publication",
    ));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if code != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn directory_names(
    directory: &File,
    maximum: usize,
) -> Result<Vec<String>, CredentialRebindErrorV1> {
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
                .map_err(|_| CredentialRebindErrorV1::Corrupt)?;
            if name == "." || name == ".." {
                continue;
            }
            if !names.insert(name.to_owned()) || names.len() > maximum {
                return Err(CredentialRebindErrorV1::Blocked(
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
fn fault_point(point: u8) -> Result<(), CredentialRebindErrorV1> {
    #[cfg(test)]
    if FAULT_AT.with(|value| {
        if value.get() == Some(point) {
            value.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(io::Error::other("C08 store test interruption").into());
    }
    let _ = point;
    Ok(())
}

#[cfg(test)]
mod data_tests;
#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;

#[cfg(not(unix))]
fn unavailable() -> CredentialRebindErrorV1 {
    CredentialRebindErrorV1::Unavailable(
        "credential rebind storage is unavailable on this platform",
    )
}

#[cfg(not(unix))]
impl C08IntentStoreV1 {
    pub(super) fn inspect(_state: &Path) -> Result<Self, CredentialRebindErrorV1> {
        Err(unavailable())
    }

    pub(super) fn refresh(&mut self) -> Result<(), CredentialRebindErrorV1> {
        Err(unavailable())
    }

    pub(super) fn reserve_operation(
        &mut self,
        _id: ManagementOperationIdV1,
    ) -> Result<(), CredentialRebindErrorV1> {
        Err(unavailable())
    }

    pub(super) fn publish(
        &mut self,
        _record: &C08StoredRecordV1,
    ) -> Result<(), CredentialRebindErrorV1> {
        Err(unavailable())
    }

    pub(super) fn archive_terminal(
        &mut self,
        _id: ManagementOperationIdV1,
    ) -> Result<(), CredentialRebindErrorV1> {
        Err(unavailable())
    }
}
