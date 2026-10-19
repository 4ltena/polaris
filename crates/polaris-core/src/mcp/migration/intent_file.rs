//! Descriptor-owned external migration intent, outside the activation payload.
//! Records are observations only: this owner grants no stopping, retirement or
//! activation capability. The coordinator must bind them to actual trees and
//! documents before advancing. Every update preserves the displaced old file.
use super::super::entry_gate::{EntryAnchorV2, EntryMigrationGuardV2, child, private_file};
use polaris_mcp::migration_observation::{ExternalMigrationIntentV2, MigrationIntentBindingV2};
use polaris_mcp::{Error, admission::same_metadata, registration::MAX_STORE_BYTES};
use std::{
    ffi::CString,
    fs::{File, Metadata},
    io::Write,
    os::{fd::AsRawFd, unix::fs::FileExt},
};

const PHASES: &[&str] = &[
    "retire-prepared",
    "retired",
    "staging",
    "prepared-activation",
];

pub(super) struct ExternalIntentFileV2 {
    parent: File,
    head: IntentPin,
    history: Vec<IntentPin>,
    record: ExternalMigrationIntentV2,
}

struct IntentPin {
    name: String,
    file: File,
    metadata: Metadata,
    bytes: Vec<u8>,
}

impl ExternalIntentFileV2 {
    pub(super) fn create(
        guard: &EntryMigrationGuardV2,
        record: ExternalMigrationIntentV2,
    ) -> Result<Self, Error> {
        if !matches!(record, ExternalMigrationIntentV2::RetirePrepared { .. }) {
            return Err(Error::Protocol);
        }
        verify_binding(guard, &record)?;
        let id = &binding(&record).migration_id;
        for phase in PHASES {
            absent(&guard.parent, &archive_name(id, phase))?;
        }
        let head = IntentPin::create(&guard.parent, head_name(id), record.encode()?)?;
        let value = Self {
            parent: guard.parent.try_clone().map_err(|_| Error::Storage)?,
            head,
            history: vec![],
            record,
        };
        value.revalidate(guard)?;
        Ok(value)
    }

    /// Open the exact head and all embedded predecessor records. A partial
    /// candidate, missing archive or substituted archive blocks automatic use.
    pub(super) fn open(guard: &EntryMigrationGuardV2) -> Result<Self, Error> {
        let (_, id, _, _) = guard.verified_binding().map_err(|_| Error::Protection)?;
        let head = IntentPin::open(&guard.parent, head_name(id))?;
        let record = ExternalMigrationIntentV2::decode(&head.bytes)?;
        verify_binding(guard, &record)?;
        let mut expected = record.clone();
        let mut history = Vec::new();
        while let Some(previous) = predecessor(&expected) {
            let pin = IntentPin::open(&guard.parent, archive_name(id, phase(&previous)))?;
            if ExternalMigrationIntentV2::decode(&pin.bytes)? != previous {
                return Err(Error::Protection);
            }
            history.push(pin);
            expected = previous;
        }
        history.reverse();
        let value = Self {
            parent: guard.parent.try_clone().map_err(|_| Error::Storage)?,
            head,
            history,
            record,
        };
        value.revalidate(guard)?;
        Ok(value)
    }

    pub(super) fn record(&self) -> &ExternalMigrationIntentV2 {
        &self.record
    }

    pub(super) fn revalidate(&self, guard: &EntryMigrationGuardV2) -> Result<(), Error> {
        let held = EntryAnchorV2::of(&self.parent.metadata().map_err(|_| Error::Storage)?);
        let named = EntryAnchorV2::of(&guard.parent.metadata().map_err(|_| Error::Storage)?);
        if (held.device, held.inode, held.uid, held.mode)
            != (named.device, named.inode, named.uid, named.mode)
        {
            return Err(Error::Protection);
        }
        verify_binding(guard, &self.record)?;
        for pin in &self.history {
            pin.verify(&guard.parent)?;
        }
        self.head.verify(&guard.parent)?;
        let id = &binding(&self.record).migration_id;
        for phase in &PHASES[self.history.len()..] {
            absent(&guard.parent, &archive_name(id, phase))?;
        }
        verify_binding(guard, &self.record)
    }

    /// Consume the writer on failure: a partially exchanged state cannot be
    /// advanced again through a stale in-memory witness. Reopening inspects the
    /// complete history, including any interrupted candidate, without repair.
    #[cfg(test)]
    pub(super) fn advance(
        self,
        guard: &EntryMigrationGuardV2,
        next: ExternalMigrationIntentV2,
    ) -> Result<Self, Error> {
        self.advance_with_check(guard, next, || Ok(()))
    }

    pub(super) fn advance_with_check(
        mut self,
        guard: &EntryMigrationGuardV2,
        next: ExternalMigrationIntentV2,
        before_exchange: impl FnOnce() -> Result<(), Error>,
    ) -> Result<Self, Error> {
        self.revalidate(guard)?;
        verify_binding(guard, &next)?;
        if predecessor(&next).as_ref() != Some(&self.record) {
            return Err(Error::Protocol);
        }
        let name = archive_name(&binding(&self.record).migration_id, phase(&self.record));
        let mut candidate = IntentPin::create(&guard.parent, name.clone(), next.encode()?)?;
        // Candidate creation is our one allowed new sibling. Recheck existing
        // pins and binding rather than treating it as a restartable archive.
        for pin in &self.history {
            pin.verify(&guard.parent)?;
        }
        self.head.verify(&guard.parent)?;
        verify_binding(guard, &self.record)?;
        before_exchange()?;
        #[cfg(test)]
        BEFORE_EXCHANGE.with(|slot| slot.borrow_mut().take().map_or(Ok(()), |hook| hook()))?;
        exchange(&guard.parent, &self.head.name, &candidate.name)?;
        guard.parent.sync_all().map_err(|_| Error::Storage)?;
        // Preserve and inspect the actual displaced object. No unlink or
        // rollback erases evidence if an external writer raced the exchange.
        self.head.adopt_renamed(&guard.parent, name)?;
        candidate.adopt_renamed(&guard.parent, self.head_name())?;
        self.head.file.sync_all().map_err(|_| Error::Storage)?;
        candidate.file.sync_all().map_err(|_| Error::Storage)?;
        guard.parent.sync_all().map_err(|_| Error::Storage)?;
        self.history.push(self.head);
        self.head = candidate;
        self.record = next;
        self.revalidate(guard)?;
        Ok(self)
    }

    fn head_name(&self) -> String {
        head_name(&binding(&self.record).migration_id)
    }
}

impl IntentPin {
    fn create(parent: &File, name: String, bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() > MAX_STORE_BYTES {
            return Err(Error::Limit);
        }
        let mut file = child(parent, &name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)
            .map_err(|_| Error::Protection)?;
        private_file(&file).map_err(|_| Error::Protection)?;
        verify_mount(parent, &file)?;
        file.write_all(&bytes).map_err(|_| Error::Storage)?;
        file.sync_all().map_err(|_| Error::Storage)?;
        parent.sync_all().map_err(|_| Error::Storage)?;
        let value = Self {
            name,
            metadata: file.metadata().map_err(|_| Error::Storage)?,
            file,
            bytes,
        };
        value.verify(parent)?;
        Ok(value)
    }
    fn open(parent: &File, name: String) -> Result<Self, Error> {
        let file = child(parent, &name, libc::O_RDONLY).map_err(|_| Error::Protection)?;
        private_file(&file).map_err(|_| Error::Protection)?;
        verify_mount(parent, &file)?;
        let metadata = file.metadata().map_err(|_| Error::Storage)?;
        let bytes = read(&file, &metadata)?;
        let value = Self {
            name,
            file,
            metadata,
            bytes,
        };
        value.verify(parent)?;
        Ok(value)
    }
    fn verify(&self, parent: &File) -> Result<(), Error> {
        let named = child(parent, &self.name, libc::O_RDONLY).map_err(|_| Error::Protection)?;
        for file in [&self.file, &named] {
            private_file(file).map_err(|_| Error::Protection)?;
            verify_mount(parent, file)?;
            if !same_metadata(
                &self.metadata,
                &file.metadata().map_err(|_| Error::Storage)?,
            ) || read(file, &self.metadata)? != self.bytes
            {
                return Err(Error::Protection);
            }
            verify_mount(parent, file)?;
        }
        Ok(())
    }
    fn adopt_renamed(&mut self, parent: &File, name: String) -> Result<(), Error> {
        let named = child(parent, &name, libc::O_RDONLY).map_err(|_| Error::Protection)?;
        verify_mount(parent, &named)?;
        verify_mount(parent, &self.file)?;
        let fresh = self.file.metadata().map_err(|_| Error::Storage)?;
        // Rename can change ctime. Identity, protection and exact non-secret
        // bytes remain fixed; fresh metadata is sealed before accepting it.
        if EntryAnchorV2::of(&fresh) != EntryAnchorV2::of(&self.metadata)
            || fresh.len() != self.metadata.len()
            || !same_metadata(&fresh, &named.metadata().map_err(|_| Error::Storage)?)
            || read(&named, &fresh)? != self.bytes
            || read(&self.file, &fresh)? != self.bytes
        {
            return Err(Error::Protection);
        }
        self.name = name;
        self.metadata = fresh;
        self.verify(parent)
    }
}

pub(super) fn verify_mount(parent: &File, file: &File) -> Result<(), Error> {
    use polaris_tools::native_mount::NativeMountIdentityV2;
    let expected = NativeMountIdentityV2::inspect(parent).map_err(|_| Error::Protection)?;
    if NativeMountIdentityV2::inspect(file).map_err(|_| Error::Protection)? != expected {
        return Err(Error::Protection);
    }
    #[cfg(test)]
    if let Some(check) = MOUNT_CHECK.with(|slot| slot.borrow_mut().take()) {
        check(parent, file)?;
    }
    Ok(())
}

fn read(file: &File, expected: &Metadata) -> Result<Vec<u8>, Error> {
    private_file(file).map_err(|_| Error::Protection)?;
    if expected.len() > MAX_STORE_BYTES as u64 {
        return Err(Error::Limit);
    }
    if !same_metadata(expected, &file.metadata().map_err(|_| Error::Storage)?) {
        return Err(Error::Protection);
    }
    let mut bytes = vec![0; expected.len() as usize];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|_| Error::Protection)?;
    let mut extra = [0u8; 1];
    if file
        .read_at(&mut extra, expected.len())
        .map_err(|_| Error::Protection)?
        != 0
        || !same_metadata(expected, &file.metadata().map_err(|_| Error::Storage)?)
    {
        return Err(Error::Protection);
    }
    Ok(bytes)
}
fn head_name(id: &str) -> String {
    format!("mcp-migration.v2-{id}.json")
}
fn archive_name(id: &str, phase: &str) -> String {
    format!(".mcp-migration.v2-{id}.from-{phase}.json")
}
fn phase(value: &ExternalMigrationIntentV2) -> &'static str {
    match value {
        ExternalMigrationIntentV2::RetirePrepared { .. } => "retire-prepared",
        ExternalMigrationIntentV2::Retired { .. } => "retired",
        ExternalMigrationIntentV2::Staging { .. } => "staging",
        ExternalMigrationIntentV2::PreparedActivation { .. } => "prepared-activation",
        ExternalMigrationIntentV2::ActiveV2 { .. } => "active",
    }
}
fn predecessor(value: &ExternalMigrationIntentV2) -> Option<ExternalMigrationIntentV2> {
    use ExternalMigrationIntentV2::*;
    match value {
        RetirePrepared { .. } => None,
        Retired { record } => Some(RetirePrepared {
            record: super::super::records::RetirePreparedRecordV2 {
                binding: record.binding.clone(),
            },
        }),
        Staging { record } => Some(Retired {
            record: record.retired.clone(),
        }),
        PreparedActivation { record } => Some(Staging {
            record: record.staging.clone(),
        }),
        ActiveV2 { record } => Some(PreparedActivation {
            record: record.prepared.clone(),
        }),
    }
}
fn binding(value: &ExternalMigrationIntentV2) -> &MigrationIntentBindingV2 {
    use ExternalMigrationIntentV2::*;
    match value {
        RetirePrepared { record } => &record.binding,
        Retired { record } => &record.binding,
        Staging { record } => &record.retired.binding,
        PreparedActivation { record } => &record.staging.retired.binding,
        ActiveV2 { record } => &record.prepared.staging.retired.binding,
    }
}
fn verify_binding(
    guard: &EntryMigrationGuardV2,
    record: &ExternalMigrationIntentV2,
) -> Result<(), Error> {
    record.validate()?;
    let (_, id, manifest, hash) = guard.verified_binding().map_err(|_| Error::Protection)?;
    let candidate = binding(record);
    let parent = EntryAnchorV2::of(&guard.parent.metadata().map_err(|_| Error::Storage)?);
    let recorded = &candidate.canonical_parent_anchor;
    if id != candidate.migration_id
        || *manifest != candidate.entry_manifest
        || hash != candidate.entry_manifest_hash
        || (parent.device, parent.inode, parent.uid, parent.mode)
            != (recorded.device, recorded.inode, recorded.uid, recorded.mode)
    {
        return Err(Error::Protection);
    }
    Ok(())
}
fn absent(parent: &File, name: &str) -> Result<(), Error> {
    match child(parent, name, libc::O_RDONLY | libc::O_NONBLOCK) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(Error::Protection),
    }
}
fn exchange(parent: &File, left: &str, right: &str) -> Result<(), Error> {
    let left = CString::new(left).map_err(|_| Error::Protection)?;
    let right = CString::new(right).map_err(|_| Error::Protection)?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            left.as_ptr(),
            parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    return Err(Error::Protection);
    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    if result != 0 {
        return Err(Error::Protection);
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static BEFORE_EXCHANGE: std::cell::RefCell<Option<Box<dyn FnOnce() -> Result<(), Error>>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
thread_local! { pub(super) static MOUNT_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce(&File, &File) -> Result<(), Error>>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
#[path = "intent_file/tests.rs"]
mod tests;
