//! Descriptor-bound durable ledger payload. This layer checks a caller-supplied
//! epoch and the current payload; it does not establish retired-tree projection,
//! production stop proof, external intent or entry-gate admission.
use super::{
    CredentialLedgerEventV1, CredentialLedgerRecordV1, CredentialLedgerSnapshotV1, LedgerEpochV1,
    MAX_LEDGER_BYTES, MAX_LEDGER_RECORD_BYTES,
    payload_head::{LedgerHeadV1, MAX_HEAD_BYTES, bytes_hash, empty_payload_head},
};
use crate::{
    Error,
    entry_gate::{EntryAnchorV2, child, owned_directory, physical_directory, private_file},
};
use std::{
    collections::BTreeSet,
    ffi::CStr,
    fs::{File, Metadata},
    io,
    os::{
        fd::{AsRawFd, IntoRawFd},
        unix::fs::{FileExt, MetadataExt},
    },
    path::{Component, Path},
};

const ROOT: &str = "mcp-v2";
const LEDGER: &str = "ledger";
const LOCK: &str = "mcp-credential-ledger-v2.lock";
const EPOCH: &str = "epoch.json";
const HISTORY: &str = "history.jsonl";
const HEAD: &str = "head.json";
const HEAD_TEMP: &str = "head.pending";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LedgerOpenModeV1 {
    ReadOnly,
    Mutating,
}

struct DirectoryPin {
    name: String,
    file: File,
    anchor: EntryAnchorV2,
}
struct FilePin {
    file: File,
    anchor: EntryAnchorV2,
}
impl FilePin {
    fn open(parent: &File, name: &str, flags: i32) -> Result<Self, Error> {
        let file = child(parent, name, flags).map_err(open_error)?;
        Self::of(file)
    }
    fn of(file: File) -> Result<Self, Error> {
        private_file(&file).map_err(|_| Error::Protection)?;
        let anchor = EntryAnchorV2::of(&file.metadata().map_err(storage)?);
        Ok(Self { file, anchor })
    }
    fn read(&self, parent: &File, name: &str, maximum: usize) -> Result<Vec<u8>, Error> {
        private_file(&self.file).map_err(|_| Error::Protection)?;
        let before = self.file.metadata().map_err(storage)?;
        if EntryAnchorV2::of(&before) != self.anchor {
            return Err(Error::Protection);
        }
        if before.len() > maximum as u64 {
            return Err(Error::Limit);
        }
        let mut bytes = vec![0; before.len() as usize];
        let mut offset = 0;
        while offset < bytes.len() {
            match self.file.read_at(&mut bytes[offset..], offset as u64) {
                Ok(0) => return Err(Error::RecoveryRequired),
                Ok(count) => offset += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(storage(error)),
            }
        }
        let after = self.file.metadata().map_err(storage)?;
        let named = child(parent, name, libc::O_RDONLY).map_err(open_error)?;
        private_file(&named).map_err(|_| Error::Protection)?;
        if !stable_file(&before, &after)
            || !stable_file(&before, &named.metadata().map_err(storage)?)
        {
            return Err(Error::Protection);
        }
        Ok(bytes)
    }
}

pub(crate) struct CredentialLedgerFileStoreV1 {
    ancestry: Vec<DirectoryPin>,
    root: File,
    root_anchor: EntryAnchorV2,
    ledger: File,
    ledger_anchor: EntryAnchorV2,
    lock: FilePin,
    epoch: FilePin,
    epoch_bytes: Vec<u8>,
    history: FilePin,
    history_bytes: Vec<u8>,
    head: FilePin,
    head_bytes: Vec<u8>,
    snapshot: CredentialLedgerSnapshotV1,
    mode: LedgerOpenModeV1,
    reservation: Option<(usize, usize)>,
    poisoned: bool,
    #[cfg(test)]
    fault: Option<u8>,
    #[cfg(test)]
    fault_reached: std::cell::Cell<Option<u8>>,
}

impl CredentialLedgerFileStoreV1 {
    /// Opens existing files only. `expected_epoch` must come from the migration
    /// coordinator's external evidence; equality here is not an admission proof.
    pub(crate) fn inspect(
        parent: &Path,
        expected_epoch: &LedgerEpochV1,
        mode: LedgerOpenModeV1,
    ) -> Result<Self, Error> {
        expected_epoch.validate()?;
        let epoch_bytes = expected_epoch.encode()?;
        let ancestry = pin_ancestry(parent)?;
        let parent_fd = &ancestry.last().ok_or(Error::Protection)?.file;
        owned_directory(parent_fd, false).map_err(|_| Error::Protection)?;
        if !same_directory(
            &EntryAnchorV2::of(&parent_fd.metadata().map_err(storage)?),
            &expected_epoch.canonical_parent_anchor,
        ) {
            return Err(Error::Protection);
        }
        let lock = FilePin::open(parent_fd, LOCK, libc::O_RDONLY)?;
        if lock.file.metadata().map_err(storage)?.len() != 0 {
            return Err(Error::Protection);
        }
        let operation = match mode {
            LedgerOpenModeV1::ReadOnly => libc::LOCK_SH,
            LedgerOpenModeV1::Mutating => libc::LOCK_EX,
        };
        if unsafe { libc::flock(lock.file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return Err(if error.kind() == io::ErrorKind::WouldBlock {
                Error::RevisionConflict
            } else {
                Error::Storage
            });
        }
        let root =
            child(parent_fd, ROOT, libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
        let root_anchor = private_directory(&root)?;
        if root_anchor != expected_epoch.active_v2_root_anchor {
            return Err(Error::Protection);
        }
        let ledger =
            child(&root, LEDGER, libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
        let ledger_anchor = private_directory(&ledger)?;
        inventory(&ledger, &[EPOCH, HISTORY, HEAD])?;
        let epoch = FilePin::open(&ledger, EPOCH, libc::O_RDONLY)?;
        if epoch.read(&ledger, EPOCH, MAX_LEDGER_RECORD_BYTES)? != epoch_bytes {
            return Err(Error::RecoveryRequired);
        }
        let history = FilePin::open(
            &ledger,
            HISTORY,
            if mode == LedgerOpenModeV1::Mutating {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            },
        )?;
        let history_bytes = history.read(&ledger, HISTORY, MAX_LEDGER_BYTES)?;
        let head = FilePin::open(&ledger, HEAD, libc::O_RDONLY)?;
        let head_bytes = head.read(&ledger, HEAD, MAX_HEAD_BYTES)?;
        let snapshot = decode_payload(&epoch_bytes, &history_bytes, &head_bytes)?;
        let store = Self {
            ancestry,
            root,
            root_anchor,
            ledger,
            ledger_anchor,
            lock,
            epoch,
            epoch_bytes,
            history,
            history_bytes,
            head,
            head_bytes,
            snapshot,
            mode,
            reservation: None,
            poisoned: false,
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            fault_reached: std::cell::Cell::new(None),
        };
        store.verify_current()?;
        Ok(store)
    }

    /// Revalidates the cached view under the retained lock. A poisoned handle
    /// cannot turn a pre-append DTO into fresh durable evidence.
    pub(crate) fn snapshot(&self) -> Result<&CredentialLedgerSnapshotV1, Error> {
        self.verify_current()?;
        Ok(&self.snapshot)
    }

    /// Reservation failure is pre-write and leaves a valid handle usable. A
    /// successful reservation replaces the remaining budget on this handle.
    pub(crate) fn reserve_capacity(
        &mut self,
        records: usize,
        operations: usize,
    ) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::RecoveryRequired);
        }
        if self.mode != LedgerOpenModeV1::Mutating {
            return Err(Error::Protection);
        }
        self.verify_current()?;
        self.snapshot.reserve_capacity(records, operations)?;
        if records
            .checked_mul(MAX_LEDGER_RECORD_BYTES + 1)
            .and_then(|bytes| self.snapshot.encoded_bytes.checked_add(bytes))
            .and_then(|bytes| bytes.checked_add(2 * MAX_HEAD_BYTES))
            .is_none_or(|bytes| bytes > MAX_LEDGER_BYTES)
        {
            return Err(Error::Limit);
        }
        self.reservation = Some((records, operations));
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        event: CredentialLedgerEventV1,
    ) -> Result<CredentialLedgerRecordV1, Error> {
        if self.poisoned {
            return Err(Error::RecoveryRequired);
        }
        if self.mode != LedgerOpenModeV1::Mutating {
            return Err(Error::Protection);
        }
        let new_operation = usize::from(matches!(&event, CredentialLedgerEventV1::Reserved { .. }));
        let (records, operations) = self.reservation.ok_or(Error::Limit)?;
        if records == 0 || operations < new_operation {
            return Err(Error::Limit);
        }
        let record = self.snapshot.next_record(event)?;
        let mut record_bytes = record.encode()?;
        record_bytes.push(b'\n');
        let mut history_bytes = self.history_bytes.clone();
        history_bytes.extend_from_slice(&record_bytes);
        let candidate = CredentialLedgerSnapshotV1::decode(&self.epoch_bytes, &history_bytes)?;
        let head_bytes = LedgerHeadV1::current(&candidate, &history_bytes).encode()?;
        match self.append_durable(&record_bytes, &history_bytes, &head_bytes) {
            Ok(head) => {
                self.head = head;
                self.head_bytes = head_bytes;
                self.history_bytes = history_bytes;
                self.snapshot = candidate;
                self.reservation = Some((records - 1, operations - new_operation));
                if let Err(error) = self.verify_current() {
                    self.poisoned = true;
                    return Err(error);
                }
                Ok(record)
            }
            Err(error) => {
                // No cleanup or truncation: a complete history with the old
                // head, a temp or a torn tail must block the next open.
                self.poisoned = true;
                Err(error)
            }
        }
    }

    fn parent(&self) -> &File {
        &self
            .ancestry
            .last()
            .expect("nonempty inspected ancestry")
            .file
    }
    fn verify_structure(&self, temporary: bool) -> Result<(), Error> {
        for (index, pin) in self.ancestry.iter().enumerate() {
            if !same_directory(
                &EntryAnchorV2::of(&pin.file.metadata().map_err(storage)?),
                &pin.anchor,
            ) {
                return Err(Error::Protection);
            }
            if index > 0 {
                let named = child(
                    &self.ancestry[index - 1].file,
                    &pin.name,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                )
                .map_err(open_error)?;
                if !same_directory(
                    &EntryAnchorV2::of(&named.metadata().map_err(storage)?),
                    &pin.anchor,
                ) {
                    return Err(Error::Protection);
                }
            }
        }
        owned_directory(self.parent(), false).map_err(|_| Error::Protection)?;
        for (parent, name, held, anchor) in [
            (self.parent(), ROOT, &self.root, &self.root_anchor),
            (&self.root, LEDGER, &self.ledger, &self.ledger_anchor),
        ] {
            let named =
                child(parent, name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
            let held_anchor = private_directory(held)?;
            let named_anchor = private_directory(&named)?;
            // APFS counts regular directory entries in st_nlink; Linux usually
            // does not. Only the one owned temporary may alter this count,
            // and the exact FD inventory below must still agree.
            let owned_temporary_links = name == LEDGER
                && temporary
                && anchor.links.checked_add(1) == Some(held_anchor.links);
            if !same_directory(&held_anchor, anchor)
                || named_anchor != held_anchor
                || (held_anchor.links != anchor.links && !owned_temporary_links)
            {
                return Err(Error::Protection);
            }
        }
        self.lock.read(self.parent(), LOCK, 0)?;
        inventory(
            &self.ledger,
            if temporary {
                &[EPOCH, HISTORY, HEAD, HEAD_TEMP]
            } else {
                &[EPOCH, HISTORY, HEAD]
            },
        )
    }
    fn verify_payload(
        &self,
        expected_history: &[u8],
        temporary: Option<(&FilePin, &[u8])>,
    ) -> Result<(), Error> {
        // Re-read every bound payload after the first read. Descriptor identity,
        // named identity and the whole directory closure are checked each time.
        for _ in 0..2 {
            self.verify_structure(temporary.is_some())?;
            if self
                .epoch
                .read(&self.ledger, EPOCH, MAX_LEDGER_RECORD_BYTES)?
                != self.epoch_bytes
                || self.head.read(&self.ledger, HEAD, MAX_HEAD_BYTES)? != self.head_bytes
                || self.history.read(&self.ledger, HISTORY, MAX_LEDGER_BYTES)? != expected_history
            {
                return Err(Error::RecoveryRequired);
            }
            if let Some((file, bytes)) = temporary
                && file.read(&self.ledger, HEAD_TEMP, MAX_HEAD_BYTES)? != bytes
            {
                return Err(Error::RecoveryRequired);
            }
        }
        self.verify_structure(temporary.is_some())
    }
    fn verify_current(&self) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::RecoveryRequired);
        }
        self.verify_payload(&self.history_bytes, None)?;
        decode_payload(&self.epoch_bytes, &self.history_bytes, &self.head_bytes)?;
        Ok(())
    }
    fn append_durable(&self, record: &[u8], history: &[u8], head: &[u8]) -> Result<FilePin, Error> {
        self.verify_current()?;
        write_all_at(&self.history.file, record, self.history_bytes.len() as u64)?;
        self.checkpoint(1)?;
        self.history.file.sync_all().map_err(storage)?;
        self.checkpoint(2)?;
        self.ledger.sync_all().map_err(storage)?;
        self.checkpoint(3)?;
        let temp = FilePin::open(
            &self.ledger,
            HEAD_TEMP,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )?;
        self.checkpoint(4)?;
        write_all_at(&temp.file, head, 0)?;
        self.checkpoint(5)?;
        temp.file.sync_all().map_err(storage)?;
        self.checkpoint(6)?;
        self.verify_payload(history, Some((&temp, head)))?;
        decode_payload(&self.epoch_bytes, history, head)?;
        if unsafe {
            libc::renameat(
                self.ledger.as_raw_fd(),
                c"head.pending".as_ptr(),
                self.ledger.as_raw_fd(),
                c"head.json".as_ptr(),
            )
        } != 0
        {
            return Err(Error::Storage);
        }
        self.checkpoint(7)?;
        self.ledger.sync_all().map_err(storage)?;
        self.checkpoint(8)?;
        let published = FilePin::open(&self.ledger, HEAD, libc::O_RDONLY)?;
        if published.anchor != temp.anchor
            || published.read(&self.ledger, HEAD, MAX_HEAD_BYTES)? != head
        {
            return Err(Error::RecoveryRequired);
        }
        Ok(published)
    }
    fn checkpoint(&self, point: u8) -> Result<(), Error> {
        #[cfg(test)]
        if self.fault == Some(point) {
            self.fault_reached.set(Some(point));
            return Err(Error::Storage);
        }
        let _ = point;
        Ok(())
    }
}

/// Only the future migration coordinator (or disposable tests) may call this
/// before writing the immutable final epoch. It creates no lock or epoch and
/// confers no stop proof, activation proof or production entry capability.
pub(crate) fn prepare_empty_payload(staging_root: &File, epoch_id: &str) -> Result<(), Error> {
    let root_anchor = private_directory(staging_root)?;
    let head_bytes = empty_payload_head(epoch_id)?;
    if unsafe { libc::mkdirat(staging_root.as_raw_fd(), c"ledger".as_ptr(), 0o700) } != 0 {
        return Err(Error::Storage);
    }
    staging_root.sync_all().map_err(storage)?;
    let ledger =
        child(staging_root, LEDGER, libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
    private_directory(&ledger)?;
    let history = FilePin::open(
        &ledger,
        HISTORY,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
    )?;
    history.file.sync_all().map_err(storage)?;
    let head = FilePin::open(&ledger, HEAD, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
    write_all_at(&head.file, &head_bytes, 0)?;
    head.file.sync_all().map_err(storage)?;
    ledger.sync_all().map_err(storage)?;
    staging_root.sync_all().map_err(storage)?;
    inventory(&ledger, &[HISTORY, HEAD])?;
    if !same_directory(&private_directory(staging_root)?, &root_anchor)
        || !history.read(&ledger, HISTORY, 0)?.is_empty()
        || head.read(&ledger, HEAD, MAX_HEAD_BYTES)? != head_bytes
    {
        return Err(Error::RecoveryRequired);
    }
    let named =
        child(staging_root, LEDGER, libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
    if !same_directory(&private_directory(&named)?, &private_directory(&ledger)?) {
        return Err(Error::Protection);
    }
    inventory(&ledger, &[HISTORY, HEAD])
}

fn decode_payload(
    epoch: &[u8],
    history: &[u8],
    head: &[u8],
) -> Result<CredentialLedgerSnapshotV1, Error> {
    if epoch
        .len()
        .checked_add(history.len())
        .and_then(|bytes| bytes.checked_add(head.len()))
        .is_none_or(|bytes| bytes > MAX_LEDGER_BYTES)
    {
        return Err(Error::Limit);
    }
    let snapshot = CredentialLedgerSnapshotV1::decode(epoch, history)?;
    if LedgerHeadV1::decode(head)? != LedgerHeadV1::current(&snapshot, history) {
        return Err(Error::RecoveryRequired);
    }
    Ok(snapshot)
}
fn same_directory(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    // Directory link counts differ by platform and can change when an external
    // gate replaces its files. Identity, owner and mode stay immutable.
    (a.device, a.inode, a.uid, a.mode) == (b.device, b.inode, b.uid, b.mode)
        && a.links > 0
        && b.links > 0
}
fn private_directory(file: &File) -> Result<EntryAnchorV2, Error> {
    owned_directory(file, true).map_err(|_| Error::Protection)?;
    let anchor = EntryAnchorV2::of(&file.metadata().map_err(storage)?);
    if anchor.mode != 0o700 || anchor.links == 0 {
        return Err(Error::Protection);
    }
    Ok(anchor)
}
fn stable_file(a: &Metadata, b: &Metadata) -> bool {
    EntryAnchorV2::of(a) == EntryAnchorV2::of(b)
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn pin_ancestry(path: &Path) -> Result<Vec<DirectoryPin>, Error> {
    // Reuse the gate's strict absolute, physical-path parser before retaining
    // every traversal descriptor; checking only the final path loses ancestors.
    physical_directory(path).map_err(|_| Error::Protection)?;
    if path.file_name().and_then(|name| name.to_str()) != Some(".polaris") {
        return Err(Error::Protection);
    }
    let root = File::open("/").map_err(storage)?;
    let anchor = EntryAnchorV2::of(&root.metadata().map_err(storage)?);
    let mut pins = vec![DirectoryPin {
        name: String::new(),
        file: root,
        anchor,
    }];
    for component in path.components() {
        if let Component::Normal(name) = component {
            let name = name.to_str().ok_or(Error::Protection)?;
            let file = child(
                &pins.last().ok_or(Error::Protection)?.file,
                name,
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .map_err(open_error)?;
            let anchor = EntryAnchorV2::of(&file.metadata().map_err(storage)?);
            pins.push(DirectoryPin {
                name: name.to_owned(),
                file,
                anchor,
            });
        }
    }
    // A user's home and canonical parent are owned and non-writable by others.
    if pins.len() < 3 {
        return Err(Error::Protection);
    }
    for pin in &pins[pins.len() - 2..] {
        owned_directory(&pin.file, false).map_err(|_| Error::Protection)?;
    }
    Ok(pins)
}

struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}
#[cfg(test)]
std::thread_local! {
    // A one-shot hook on the owning test thread, after the last readdir and
    // before the closing metadata check. No production injection surface.
    static INVENTORY_AFTER_EOF: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}
fn inventory(directory: &File, expected: &[&str]) -> Result<(), Error> {
    private_directory(directory)?;
    let before = directory.metadata().map_err(storage)?;
    // A fresh openat gives readdir its own cursor; dup/try_clone would share one.
    let independent =
        child(directory, ".", libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
    if !stable_file(&before, &independent.metadata().map_err(storage)?) {
        return Err(Error::Protection);
    }
    let fd = independent.into_raw_fd();
    let pointer = unsafe { libc::fdopendir(fd) };
    if pointer.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(Error::Storage);
    }
    let stream = DirectoryStream(pointer);
    let mut names = BTreeSet::new();
    loop {
        clear_errno();
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if errno() != 0 {
                return Err(Error::Storage);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() >= expected.len() {
            return Err(Error::RecoveryRequired);
        }
        let name = std::str::from_utf8(name).map_err(|_| Error::Protection)?;
        if !expected.contains(&name) || !names.insert(name.to_owned()) {
            return Err(Error::RecoveryRequired);
        }
        let file = child(directory, name, libc::O_RDONLY).map_err(open_error)?;
        private_file(&file).map_err(|_| Error::Protection)?;
    }
    #[cfg(test)]
    if let Some(hook) = INVENTORY_AFTER_EOF.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
    // File identity alone cannot seal a directory inventory: a late addition
    // can leave the same inode and already-returned names. Recheck link count,
    // size and high-resolution mtime/ctime across the whole enumeration.
    let after = directory.metadata().map_err(storage)?;
    let closing = child(directory, ".", libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
    private_directory(&closing)?;
    if !stable_file(&before, &after) || !stable_file(&before, &closing.metadata().map_err(storage)?)
    {
        return Err(Error::Protection);
    }
    if names.len() != expected.len() {
        return Err(Error::RecoveryRequired);
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
#[cfg(target_os = "linux")]
fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}
fn clear_errno() {
    unsafe {
        *errno_pointer() = 0;
    }
}
fn errno() -> libc::c_int {
    unsafe { *errno_pointer() }
}

fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> Result<(), Error> {
    write_all_at_with(bytes, offset, |bytes, offset| file.write_at(bytes, offset))
}
fn write_all_at_with(
    mut bytes: &[u8],
    mut offset: u64,
    mut write: impl FnMut(&[u8], u64) -> io::Result<usize>,
) -> Result<(), Error> {
    while !bytes.is_empty() {
        match write(bytes, offset) {
            Ok(0) => return Err(Error::Storage),
            Ok(count) => {
                offset = offset.checked_add(count as u64).ok_or(Error::Limit)?;
                bytes = &bytes[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(storage(error)),
        }
    }
    Ok(())
}
fn open_error(error: io::Error) -> Error {
    if error.kind() == io::ErrorKind::NotFound {
        Error::RecoveryRequired
    } else {
        Error::Protection
    }
}
fn storage(_: io::Error) -> Error {
    Error::Storage
}

#[cfg(test)]
mod tests;
