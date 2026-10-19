//! Read-only classification of decoded history before MCP store migration.
//! These private checks prove neither root inventory completeness nor stopping.
use super::*;
use polaris_provider::Role;
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "migration_local.rs"]
pub(in crate::desktop_store) mod local;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "migration_root.rs"]
pub(in crate::desktop_store) mod root;

pub(in crate::desktop_store) fn validate_desktop(published: &Published) -> StoreResult<()> {
    validate_ledger(published)?;
    if published
        .state
        .runs
        .iter()
        .flat_map(|run| &run.operations)
        .any(|op| (op.mcp.is_some() || op.prompt.is_some()) && mcp::unresolved(op))
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(())
}

/// The caller must supply one verified snapshot turn, including expired epochs.
/// A decoded slice is not a filesystem capability or a global zero proof.
pub(in crate::desktop_store) fn validate_local_turn(
    events: &[RawEventV2],
    project: &str,
    session: &str,
) -> StoreResult<()> {
    crate::conversation_state::validate_complete_turn_events(events.iter())
        .map_err(|_| StoreError::RecoveryRequired)?;
    let first = &events[0];
    if events
        .iter()
        .any(|event| event.epoch != first.epoch || event.turn_id != first.turn_id)
    {
        return Err(StoreError::RunConflict);
    }
    let messages = events
        .iter()
        .map(|event| event.message.clone())
        .collect::<Vec<_>>();
    validate_local_tool_history(&messages)?;
    for event in events
        .iter()
        .filter(|event| !event.starts_turn && event.message.role == Role::User)
    {
        if prompt::inspect_local_terminal(&event.message, project, session)?
            == Some(mcp::McpDisposition::OutcomeUnknown)
        {
            return Err(StoreError::RecoveryRequired);
        }
    }
    Ok(())
}

/// Shared typed tool evidence validation, without inferring prompt provenance.
pub(in crate::desktop_store) fn validate_local_tool_history(
    messages: &[Message],
) -> StoreResult<()> {
    for call in messages.iter().flat_map(|message| &message.tool_calls) {
        let alias = call.name.strip_prefix("mcp_").is_some_and(|suffix| {
            suffix.len() == 60
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        // Search/list/select return metadata, not an operation binding. Their
        // unfinished calls were already rejected by the complete-turn check.
        if !alias && !(call.name == "mcp_catalog" && call.arguments["op"] == "read") {
            continue;
        }
        let result = messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some(call.id.as_str()))
            .ok_or(StoreError::RecoveryRequired)?;
        if mcp::inspect_local_terminal(messages, result)? == mcp::McpDisposition::OutcomeUnknown {
            return Err(StoreError::RecoveryRequired);
        }
    }
    Ok(())
}

// Actual read-only session observations; the containing DesktopRoot inventory
// and all other canonical/custom/local roots remain the outer owner's gate.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::desktop_store) struct DesktopSessionTransportInspectionV2 {
    parent: disk::Directory,
    parent_metadata: std::fs::Metadata,
    directory: disk::Directory,
    directory_metadata: std::fs::Metadata,
    bucket: String,
    mount: Arc<polaris_mcp::NativeMountIdentityV2>,
    files: Vec<SessionFilePin>,
    published: Published,
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct SessionFilePin {
    name: &'static str,
    file: File,
    metadata: std::fs::Metadata,
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
const SESSION_FILES: [&str; 4] = ["desktop.jsonl", "raw.jsonl", "state.json", "writer.lock"];

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl DesktopSessionTransportInspectionV2 {
    pub(in crate::desktop_store) fn inspect(
        parent: &disk::Directory,
        bucket: &str,
    ) -> StoreResult<Self> {
        let mut remaining = u64::MAX;
        Self::inspect_bounded(parent, bucket, &mut remaining)
    }
    pub(in crate::desktop_store) fn inspect_bounded(
        parent: &disk::Directory,
        bucket: &str,
        remaining: &mut u64,
    ) -> StoreResult<Self> {
        if bucket.len() != 64
            || !bucket
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StoreError::Corrupt("desktop session bucket"));
        }
        let supplied = parent.retained_file_m2a()?;
        let parent = disk::Directory::open_owned(parent.path())?;
        let parent_file = parent.retained_file_m2a()?;
        disk::same(&supplied, &parent_file)?;
        let parent_metadata = parent_file.metadata()?;
        let mount = Arc::new(native_mount(&parent_file)?);
        let parent = parent.with_migration_mount(mount.clone())?;
        let directory = parent.child(bucket, false)?;
        let directory_file = directory.retained_file_m2a()?;
        let directory_metadata = directory_file.metadata()?;
        if native_mount(&directory_file)? != *mount || directory.names_m2a()? != SESSION_FILES {
            return Err(StoreError::RecoveryRequired);
        }
        let mut files = Vec::new();
        for name in SESSION_FILES {
            let file = directory.open(name, false, false)?;
            let metadata = file.metadata()?;
            if native_mount(&file)? != *mount {
                return Err(StoreError::RecoveryRequired);
            }
            if name == "writer.lock" {
                if metadata.len() != 0 {
                    return Err(StoreError::RecoveryRequired);
                }
                match file.try_lock_shared() {
                    Ok(()) => (),
                    Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
                    Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
                }
            }
            *remaining = remaining
                .checked_sub(metadata.len())
                .ok_or(StoreError::Capacity("desktop root inspected bytes"))?;
            files.push(SessionFilePin {
                name,
                file,
                metadata,
            });
        }
        let read = |name: &str, maximum: u64| {
            let pin = files
                .iter()
                .find(|pin| pin.name == name)
                .ok_or(StoreError::RecoveryRequired)?;
            read_held_bytes(&pin.file, &pin.metadata, maximum)
        };
        let marker_bytes = read(
            disk::MARKER,
            super::super::source_apply::MAX_D27_MARKER_BYTES as u64,
        )?;
        let raw_bytes = read(disk::RAW, 256 * 1024 * 1024)?;
        let sidecar_bytes = read(disk::SIDECAR, 256 * 1024 * 1024)?;
        let published =
            disk::decode_migration_published(&marker_bytes, &raw_bytes, &sidecar_bytes)?;
        if content_hash(published.marker.session_id.as_str().as_bytes()) != bucket {
            return Err(StoreError::RecoveryRequired);
        }
        validate_desktop(&published)?;
        #[cfg(test)]
        if let Some(hook) = AFTER_SESSION_DECODE.with(|slot| slot.borrow_mut().take()) {
            hook();
        }

        let value = Self {
            parent,
            parent_metadata,
            directory,
            directory_metadata,
            bucket: bucket.into(),
            mount,
            files,
            published,
        };
        value.verify_current()?;
        Ok(value)
    }
    pub(in crate::desktop_store) fn verified_published(&self) -> StoreResult<&Published> {
        self.verify_current()?;
        Ok(&self.published)
    }
    pub(in crate::desktop_store) fn verify_current(&self) -> StoreResult<()> {
        self.parent.verify()?;
        self.directory.verify()?;
        self.verify_directory(&self.parent, &self.parent_metadata)?;
        self.verify_directory(&self.directory, &self.directory_metadata)?;
        let named = self.parent.child(&self.bucket, false)?;
        self.verify_directory(&named, &self.directory_metadata)?;
        if self.directory.names_m2a()? != SESSION_FILES {
            return Err(StoreError::RecoveryRequired);
        }
        for pin in &self.files {
            let named = self.directory.open(pin.name, false, false)?;
            for file in [&pin.file, &named] {
                if native_mount(file)? != *self.mount
                    || file_stamp(&file.metadata()?) != file_stamp(&pin.metadata)
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
        }
        self.verify_directory(&self.directory, &self.directory_metadata)?;
        self.verify_directory(&self.parent, &self.parent_metadata)?;
        self.parent.verify()?;
        self.directory.verify()
    }
    fn verify_directory(
        &self,
        directory: &disk::Directory,
        expected: &std::fs::Metadata,
    ) -> StoreResult<()> {
        let file = directory.retained_file_m2a()?;
        if native_mount(&file)? != *self.mount
            || file_stamp(&file.metadata()?) != file_stamp(expected)
        {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(())
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::desktop_store) fn read_held_bytes(
    file: &File,
    metadata: &std::fs::Metadata,
    maximum: u64,
) -> StoreResult<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    if metadata.len() > maximum {
        return Err(StoreError::Capacity("migration held file bytes"));
    }
    if file_stamp(&file.metadata()?) != file_stamp(metadata) {
        return Err(StoreError::RecoveryRequired);
    }
    let length = usize::try_from(metadata.len()).map_err(|_| StoreError::Overflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| StoreError::Capacity("migration held bytes"))?;
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    reader
        .take(metadata.len().checked_add(1).ok_or(StoreError::Overflow)?)
        .read_to_end(&mut bytes)?;
    if bytes.len() != length || file_stamp(&file.metadata()?) != file_stamp(metadata) {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(bytes)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::desktop_store) fn native_mount(
    file: &File,
) -> StoreResult<polaris_mcp::NativeMountIdentityV2> {
    #[cfg(test)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        if REJECT_MOUNT_FILE.with(|slot| slot.get() == Some((metadata.dev(), metadata.ino()))) {
            return Err(StoreError::RecoveryRequired);
        }
    }
    polaris_mcp::NativeMountIdentityV2::inspect(file).map_err(|_| StoreError::RecoveryRequired)
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::desktop_store) fn file_stamp(
    metadata: &std::fs::Metadata,
) -> (u64, u64, u32, u32, u64, u64, i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode(),
        metadata.nlink(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
thread_local! {
    pub(in crate::desktop_store) static AFTER_SESSION_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(in crate::desktop_store) static REJECT_MOUNT_FILE: std::cell::Cell<Option<(u64,u64)>> = const { std::cell::Cell::new(None) };
}
