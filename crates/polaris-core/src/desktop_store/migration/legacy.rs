//! Existing-only legacy JSONL and pre-compaction archive observation.
//! No import, truncation, normal recovery, or prompt authorization is performed.
use super::super::writer::migration::{file_stamp, read_held_bytes, validate_local_tool_history};
use super::*;
use polaris_provider::{Message, Role};
use serde::Deserialize;
use std::{fs::Metadata, os::unix::fs::MetadataExt};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NAMES: usize = 65_536;

pub(super) struct LegacySessionsObservation {
    directories: Vec<LegacyDirectory>,
    files: Vec<LegacyFile>,
}
struct LegacyDirectory {
    path: PathPin,
    directory: Option<disk::OwnerControlledContainer>,
    metadata: Option<Metadata>,
    names: Vec<String>,
}
struct LegacyFile {
    parent: usize,
    name: String,
    file: File,
    metadata: Metadata,
}
impl LegacySessionsObservation {
    pub(super) fn inspect(data: &Path) -> StoreResult<Self> {
        Self::inspect_bounded(data, MAX_NAMES, MAX_BYTES)
    }
    fn inspect_bounded(data: &Path, mut names: usize, mut bytes: u64) -> StoreResult<Self> {
        if names > MAX_NAMES || bytes > MAX_BYTES {
            return Err(StoreError::RecoveryRequired);
        }
        let mount = PathPin::inspect(data)?.mount;
        let root = LegacyDirectory::inspect(&data.join("sessions"), &mut names)?;
        if root.path.mount != mount {
            return Err(StoreError::RecoveryRequired);
        }
        let mut value = Self {
            directories: vec![root],
            files: Vec::new(),
        };
        let children = value.directories[0].names.clone();
        for name in children {
            if let Some(id) = name.strip_suffix(".jsonl") {
                session_id(id)?;
                let raw = value.read(0, &name, &mut bytes)?;
                let text = std::str::from_utf8(&raw).map_err(|_| StoreError::RecoveryRequired)?;
                let messages = text
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(serde_json::from_str::<Message>)
                    .collect::<Result<Vec<_>, _>>()?;
                validate_messages(&messages)?;
            } else if let Some(id) = name.strip_suffix(".meta.json") {
                session_id(id)?;
                let meta: LegacyMeta = serde_json::from_slice(&value.read(0, &name, &mut bytes)?)?;
                if !Path::new(&meta.cwd).is_absolute() {
                    return Err(StoreError::RecoveryRequired);
                }
                if let Some(origin) = meta.tool_memory_origin {
                    session_id(&origin)?;
                }
                // Timestamp is historical metadata, never an execution grant.
                let _ = meta.started_at_millis;
            } else if let Some(id) = name.strip_suffix(".archive") {
                session_id(id)?;
                let archive =
                    LegacyDirectory::inspect(&data.join("sessions").join(&name), &mut names)?;
                if !archive.path.existing() || archive.path.mount != mount {
                    return Err(StoreError::RecoveryRequired);
                }
                let parent = value.directories.len();
                let children = archive.names.clone();
                value.directories.push(archive);
                for file in children {
                    let snapshot: LegacyArchive =
                        serde_json::from_slice(&value.read(parent, &file, &mut bytes)?)?;
                    if snapshot.version != 1
                        || snapshot.session_id != id
                        || !Path::new(&snapshot.project_id).is_absolute()
                        || archive_timestamp(&file)? != snapshot.archived_at_millis
                    {
                        return Err(StoreError::RecoveryRequired);
                    }
                    validate_messages(&snapshot.messages)?;
                }
            } else {
                return Err(StoreError::RecoveryRequired);
            }
        }
        #[cfg(test)]
        run_hook(&AFTER_LEGACY_DECODE);
        value.verify_current()?;
        Ok(value)
    }
    fn read(&mut self, parent: usize, name: &str, remaining: &mut u64) -> StoreResult<Vec<u8>> {
        let directory = &self.directories[parent];
        let file = directory.open_file(name)?;
        let metadata = file.metadata()?;
        *remaining = remaining
            .checked_sub(metadata.len())
            .ok_or(StoreError::Capacity("legacy history bytes"))?;
        let bytes = read_held_bytes(&file, &metadata, MAX_BYTES)?;
        self.files.push(LegacyFile {
            parent,
            name: name.into(),
            file,
            metadata,
        });
        Ok(bytes)
    }
    pub(super) fn verify_current(&self) -> StoreResult<()> {
        for directory in &self.directories {
            directory.verify_current()?;
        }
        self.verify_files()?;
        #[cfg(test)]
        run_hook(&AFTER_LEGACY_FILES);
        for directory in &self.directories {
            directory.verify_current()?;
        }
        self.verify_files()
    }
    fn verify_files(&self) -> StoreResult<()> {
        for pin in &self.files {
            let named = self.directories[pin.parent].open_file(&pin.name)?;
            for file in [&pin.file, &named] {
                if file_stamp(&file.metadata()?) != file_stamp(&pin.metadata)
                    || native_mount(file)? != self.directories[pin.parent].path.mount
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
        }
        Ok(())
    }
}
impl LegacyDirectory {
    fn inspect(path: &Path, remaining: &mut usize) -> StoreResult<Self> {
        let path = PathPin::inspect(path)?;
        let directory = path
            .existing()
            .then(|| disk::OwnerControlledContainer::open_migration(&path.path))
            .transpose()?;
        let metadata = directory
            .as_ref()
            .map(|v| v.retained_file_m2a()?.metadata().map_err(StoreError::Io))
            .transpose()?;
        let names = match &directory {
            Some(v) => v.names_bounded_migration(*remaining)?,
            None => Vec::new(),
        };
        *remaining = remaining
            .checked_sub(names.len())
            .ok_or(StoreError::Capacity("legacy history names"))?;
        #[cfg(test)]
        run_hook(&AFTER_LEGACY_NAMES);
        let value = Self {
            path,
            directory,
            metadata,
            names,
        };
        value.verify_current()?;
        Ok(value)
    }
    fn open_file(&self, name: &str) -> StoreResult<File> {
        let directory = self
            .directory
            .as_ref()
            .ok_or(StoreError::RecoveryRequired)?;
        let file = child(&directory.retained_file_m2a()?, name, libc::O_RDONLY)?;
        let meta = file.metadata()?;
        // Older metadata was written with std::fs::write (usually 0644).
        // Preserve owner/no-other-write without weakening private v2 stores.
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o022 != 0
            || meta.nlink() != 1
            || native_mount(&file)? != self.path.mount
        {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(file)
    }
    fn verify_current(&self) -> StoreResult<()> {
        self.path.verify_current()?;
        if let (Some(directory), Some(metadata)) = (&self.directory, &self.metadata) {
            let held = directory.retained_file_m2a()?;
            let named = disk::OwnerControlledContainer::open_migration(&self.path.path)?
                .retained_file_m2a()?;
            for file in [&held, &named] {
                if file_stamp(&file.metadata()?) != file_stamp(metadata)
                    || native_mount(file)? != self.path.mount
                {
                    return Err(StoreError::RecoveryRequired);
                }
            }
            if directory.names_bounded_migration(self.names.len())? != self.names {
                return Err(StoreError::RecoveryRequired);
            }
            if file_stamp(&held.metadata()?) != file_stamp(metadata) {
                return Err(StoreError::RecoveryRequired);
            }
        } else if self.directory.is_some() || self.metadata.is_some() || !self.names.is_empty() {
            return Err(StoreError::RecoveryRequired);
        }
        self.path.verify_current()
    }
}
#[derive(Deserialize)]
struct LegacyMeta {
    cwd: String,
    started_at_millis: u128,
    #[serde(default, deserialize_with = "non_null_origin")]
    tool_memory_origin: Option<String>,
}
fn non_null_origin<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}
#[derive(Deserialize)]
struct LegacyArchive {
    version: u32,
    project_id: String,
    session_id: String,
    archived_at_millis: u128,
    messages: Vec<Message>,
}
fn session_id(id: &str) -> StoreResult<()> {
    // Exact legacy import namespace, not the manager's dot-capable IDs.
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(())
}
fn archive_timestamp(name: &str) -> StoreResult<u128> {
    let parts = name
        .strip_suffix(".json")
        .ok_or(StoreError::RecoveryRequired)?
        .split('-')
        .collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
        })
    {
        return Err(StoreError::RecoveryRequired);
    }
    let timestamp = parts[0]
        .parse::<u128>()
        .map_err(|_| StoreError::RecoveryRequired)?;
    let pid = parts[1]
        .parse::<u32>()
        .map_err(|_| StoreError::RecoveryRequired)?;
    parts[2]
        .parse::<u64>()
        .map_err(|_| StoreError::RecoveryRequired)?;
    if pid == 0 {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(timestamp)
}
fn validate_messages(messages: &[Message]) -> StoreResult<()> {
    // Legacy import treats every User as a boundary. Validate each span so
    // legitimate provider call-ID reuse between turns does not conflate proof.
    // A User inserted while calls remain pending still rejects the prior span.
    let mut start = 0;
    for end in 1..=messages.len() {
        if end == messages.len() || messages[end].role == Role::User {
            let span = &messages[start..end];
            crate::conversation_state::validate_complete_messages(span.iter())
                .map_err(|_| StoreError::RecoveryRequired)?;
            validate_local_tool_history(span)?;
            start = end;
        }
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    static AFTER_LEGACY_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_LEGACY_FILES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_LEGACY_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
