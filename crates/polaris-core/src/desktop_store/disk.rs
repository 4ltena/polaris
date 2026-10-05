//! 公開prefixだけの検証と、両log同期後の単一marker置換。失敗注入は試験内に閉じる。

use super::model::*;
use crate::conversation_state::{RawEventV2, content_hash};
use polaris_desktop_protocol::ids::{DecimalU64, ProjectId, SessionId};
use polaris_provider::Role;
use serde::Serialize;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

#[cfg(test)]
thread_local! {
    pub(super) static M2A_MUTATION_CALLS: std::cell::Cell<[u64; 3]> = const { std::cell::Cell::new([0; 3]) };
    pub(super) static M2A_REMOVE_NEXT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(super) static M2A_NEXT_UID: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    pub(super) static M2A_NEXT_RACE: std::cell::RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
}
fn record_m2a_mutation(kind: usize) {
    #[cfg(test)]
    M2A_MUTATION_CALLS.with(|calls| {
        let mut value = calls.get();
        value[kind] += 1;
        calls.set(value);
    });
    #[cfg(not(test))]
    let _ = kind;
}

pub(super) const RAW: &str = "raw.jsonl";
pub(super) const SIDECAR: &str = "desktop.jsonl";
pub(super) const MARKER: &str = "state.json";

pub(super) fn supported() -> StoreResult<()> {
    if cfg!(unix) {
        Ok(())
    } else {
        Err(StoreError::UnsupportedPlatform)
    }
}

/// An owned directory capability. All child I/O is relative to its pinned fd.
#[derive(Debug)]
pub(super) struct Directory {
    path: std::path::PathBuf,
    file: File,
}
impl Directory {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn retained_file_m2a(&self) -> StoreResult<File> {
        self.verify()?;
        self.file.try_clone().map_err(StoreError::Io)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn capacity_m2a(&self) -> StoreResult<(u64, u64)> {
        use std::os::fd::AsRawFd;
        self.verify()?;
        let mut value = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: live pinned directory FD and writable statvfs storage.
        if unsafe { libc::fstatvfs(self.file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let value = unsafe { value.assume_init() };
        let unit = value.f_frsize as u64;
        if unit == 0 {
            return Err(StoreError::Corrupt("desktop allocation unit"));
        }
        let available = (value.f_bavail as u64)
            .checked_mul(unit)
            .ok_or(StoreError::Overflow)?;
        self.verify()?;
        Ok((unit, available))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn require_next_absent_m2a(&self) -> StoreResult<()> {
        use std::os::fd::AsRawFd;
        self.verify()?;
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: live directory FD, fixed component, writable stat storage.
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                c"state.next".as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Err(StoreError::RecoveryRequired);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
        self.verify()
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    #[cfg(unix)]
    pub(super) fn open_owned(path: &Path) -> StoreResult<Self> {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        };
        if !path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || fs::canonicalize(path)?.as_os_str() != path.as_os_str()
        {
            return Err(StoreError::Corrupt(
                "保存rootは絶対canonical pathが必要です",
            ));
        }
        let mut file = File::open("/")?;
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                let name = std::ffi::CString::new(name.as_bytes())
                    .map_err(|_| StoreError::Corrupt("保存path"))?;
                // SAFETY: valid live directory fd and NUL-terminated component; owned result.
                let fd = unsafe {
                    libc::openat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(open_error());
                }
                file = unsafe { File::from_raw_fd(fd) };
            }
        }
        owned_metadata(&file.metadata()?, true)?;
        let this = Self {
            path: path.to_owned(),
            file,
        };
        this.check_path_identity()?;
        Ok(this)
    }
    #[cfg(not(unix))]
    pub(super) fn open_owned(_: &Path) -> StoreResult<Self> {
        Err(StoreError::UnsupportedPlatform)
    }
    /// Identity of the pinned private directory, after checking its name.
    #[cfg(unix)]
    pub(super) fn identity(&self) -> StoreResult<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        self.verify()?;
        let metadata = self.file.metadata()?;
        owned_metadata(&metadata, true)?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    pub(super) fn identity(&self) -> StoreResult<(u64, u64)> {
        Err(StoreError::UnsupportedPlatform)
    }
    pub(super) fn verify(&self) -> StoreResult<()> {
        let current = Self::open_owned(&self.path)?;
        same(&self.file, &current.file)
    }
    #[cfg(unix)]
    fn check_path_identity(&self) -> StoreResult<()> {
        use std::os::unix::fs::MetadataExt;
        let current = fs::symlink_metadata(&self.path)?;
        let pinned = self.file.metadata()?;
        if !current.is_dir() || current.dev() != pinned.dev() || current.ino() != pinned.ino() {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(())
    }
    #[cfg(unix)]
    pub(super) fn child(&self, name: &str, create: bool) -> StoreResult<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        self.verify()?;
        let component = component(name)?;
        if create {
            record_m2a_mutation(2);
        }
        if create && unsafe { libc::mkdirat(self.file.as_raw_fd(), component.as_ptr(), 0o700) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if create {
            self.sync()?;
        }
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(open_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        owned_metadata(&file.metadata()?, true)?;
        let child = Self {
            path: self.path.join(name),
            file,
        };
        self.verify()?;
        child.verify()?;
        Ok(child)
    }
    /// Open an existing child or create it as a private directory.  Unlike
    /// `child(_, true)`, an existing directory is an idempotent success.
    #[cfg(unix)]
    pub(super) fn ensure_child(&self, name: &str) -> StoreResult<Self> {
        record_m2a_mutation(2);
        use std::os::fd::AsRawFd;
        self.verify()?;
        let component = component(name)?;
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), component.as_ptr(), 0o700) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        } else {
            self.sync()?;
        }
        self.child(name, false)
    }
    #[cfg(not(unix))]
    pub(super) fn ensure_child(&self, _: &str) -> StoreResult<Self> {
        Err(StoreError::UnsupportedPlatform)
    }
    #[cfg(not(unix))]
    pub(super) fn child(&self, _: &str, _: bool) -> StoreResult<Self> {
        Err(StoreError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    pub(super) fn open(&self, name: &str, write: bool, create: bool) -> StoreResult<File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        self.verify()?;
        let name = component(name)?;
        let flags = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let flags = flags
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_CREAT | libc::O_EXCL
            } else {
                0
            };
        if create {
            record_m2a_mutation(2);
        }
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(open_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        owned_metadata(&file.metadata()?, false)?;
        self.verify()?;
        Ok(file)
    }
    #[cfg(not(unix))]
    pub(super) fn open(&self, _: &str, _: bool, _: bool) -> StoreResult<File> {
        Err(StoreError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    pub(super) fn remove_next(&self) -> StoreResult<()> {
        #[cfg(test)]
        M2A_REMOVE_NEXT_CALLS.with(|n| n.set(n.get() + 1));
        use std::os::fd::AsRawFd;
        match self.open("state.next", false, false) {
            Ok(_) => (),
            Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        record_m2a_mutation(2);
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), c"state.next".as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    pub(super) fn remove_next(&self) -> StoreResult<()> {
        Err(StoreError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    pub(super) fn publish_next(&self) -> StoreResult<()> {
        record_m2a_mutation(2);
        use std::os::fd::AsRawFd;
        self.verify()?;
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                c"state.next".as_ptr(),
                self.file.as_raw_fd(),
                c"state.json".as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        self.verify()
    }
    /// Atomically replace one pinned directory entry with another. The source
    /// must already be a fully synced private file in this directory.
    #[cfg(unix)]
    pub(super) fn replace(&self, source: &str, target: &str) -> StoreResult<()> {
        record_m2a_mutation(2);
        use std::os::fd::AsRawFd;
        self.verify()?;
        let source = component(source)?;
        let target = component(target)?;
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                source.as_ptr(),
                self.file.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        self.verify()
    }
    #[cfg(not(unix))]
    pub(super) fn replace(&self, _: &str, _: &str) -> StoreResult<()> {
        Err(StoreError::UnsupportedPlatform)
    }
    #[cfg(not(unix))]
    pub(super) fn publish_next(&self) -> StoreResult<()> {
        Err(StoreError::UnsupportedPlatform)
    }
    pub(super) fn sync(&self) -> StoreResult<()> {
        self.verify()?;
        record_m2a_mutation(1);
        self.file.sync_all()?;
        self.verify()
    }

    /// Publish a hard link only when the destination does not already exist.
    /// The caller verifies an existing object before treating EEXIST as an
    /// idempotent success.
    #[cfg(unix)]
    pub(super) fn link_no_replace(&self, source: &str, target: &str) -> StoreResult<bool> {
        record_m2a_mutation(2);
        use std::os::fd::AsRawFd;
        self.verify()?;
        let source = component(source)?;
        let target = component(target)?;
        if unsafe {
            libc::linkat(
                self.file.as_raw_fd(),
                source.as_ptr(),
                self.file.as_raw_fd(),
                target.as_ptr(),
                0,
            )
        } == 0
        {
            self.verify()?;
            Ok(true)
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(error.into())
            }
        }
    }
    #[cfg(not(unix))]
    pub(super) fn link_no_replace(&self, _: &str, _: &str) -> StoreResult<bool> {
        Err(StoreError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    pub(super) fn unlink_file(&self, name: &str) -> StoreResult<()> {
        record_m2a_mutation(2);
        use std::os::fd::AsRawFd;
        self.verify()?;
        let name = component(name)?;
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.verify()
    }
    #[cfg(not(unix))]
    pub(super) fn unlink_file(&self, _: &str) -> StoreResult<()> {
        Err(StoreError::UnsupportedPlatform)
    }
}
#[cfg(unix)]
fn component(name: &str) -> StoreResult<std::ffi::CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(StoreError::Corrupt("保存component"));
    }
    std::ffi::CString::new(name).map_err(|_| StoreError::Corrupt("保存component"))
}
#[cfg(unix)]
fn owned_metadata(meta: &fs::Metadata, directory: bool) -> StoreResult<()> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || (directory && !meta.is_dir())
        || (!directory && (!meta.is_file() || meta.nlink() != 1))
    {
        return Err(StoreError::Corrupt(
            "保存対象の所有者・権限・種類が不正です",
        ));
    }
    Ok(())
}
pub(super) fn same(a: &File, b: &File) -> StoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = a.metadata()?;
        let b = b.metadata()?;
        if a.dev() != b.dev() || a.ino() != b.ino() {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        Err(StoreError::UnsupportedPlatform)
    }
}

pub(super) fn read_marker(dir: &Directory) -> StoreResult<Marker> {
    let mut bytes = Vec::new();
    dir.open(MARKER, false, false)?.read_to_end(&mut bytes)?;
    #[derive(serde::Deserialize)]
    struct Version {
        schema_version: u32,
    }
    let version: Version = serde_json::from_slice(&bytes)?;
    if version.schema_version != 3 {
        return Err(StoreError::UnsupportedVersion(version.schema_version));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn prefix(
    dir: &Directory,
    name: &str,
    offset: DecimalU64,
    hash: &str,
) -> StoreResult<Vec<u8>> {
    let file = dir.open(name, false, false)?;
    if file.metadata()?.len() < offset.get() {
        return Err(StoreError::Corrupt("公開offsetより短いlog"));
    }
    let mut bytes = Vec::new();
    file.take(offset.get()).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != offset.get() || content_hash(&bytes) != hash {
        return Err(StoreError::Corrupt("公開prefixのhash不一致"));
    }
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        return Err(StoreError::Corrupt("公開prefixの途中行"));
    }
    Ok(bytes)
}

pub(super) fn load(
    dir: &Directory,
    project: &ProjectId,
    session: &SessionId,
) -> StoreResult<Published> {
    let marker = read_marker(dir)?;
    if &marker.session_id != session || &marker.project_id != project {
        return Err(StoreError::TargetMismatch);
    }
    let raw_bytes = prefix(dir, RAW, marker.raw_offset, &marker.raw_hash)?;
    let side_bytes = prefix(dir, SIDECAR, marker.sidecar_offset, &marker.sidecar_hash)?;
    decode_prefixes_m2a(marker, &raw_bytes, &side_bytes)
}

fn decode_prefixes_m2a(
    marker: Marker,
    raw_bytes: &[u8],
    side_bytes: &[u8],
) -> StoreResult<Published> {
    let mut raw = Vec::<RawEventV2>::new();
    for line in raw_bytes.split_inclusive(|b| *b == b'\n') {
        let event: RawEventV2 = serde_json::from_slice(line)?;
        let prior = raw.last();
        if event.schema_version != 2
            || event.sequence
                != (raw.len() as u64)
                    .checked_add(1)
                    .ok_or(StoreError::Overflow)?
            || event.epoch != marker.epoch.get()
            || event.turn_id == 0
            || (event.starts_turn && event.message.role != Role::User)
            || prior.is_none() && !event.starts_turn
            || prior.is_some_and(|p| {
                if event.starts_turn {
                    p.turn_id.checked_add(1) != Some(event.turn_id)
                } else {
                    p.turn_id != event.turn_id
                }
            })
        {
            return Err(StoreError::Corrupt("原文イベントの版・順序・ターン境界"));
        }
        raw.push(event);
    }
    let mut latest: Option<Sidecar> = None;
    let mut observed_v2 = false;
    for (revision, line) in side_bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        let (state, version) = decode_sidecar_wire(line)?;
        if observed_v2 && version == 1 {
            return Err(StoreError::Corrupt("desktop sidecar version rollback"));
        }
        observed_v2 |= version == 2;
        super::children::validate(&state)?;
        super::source_apply::validate(&state)?;
        super::writer::mcp::validate_state(&state)?;
        super::writer::prompt::validate_state(&state)?;
        if let Some(prior) = &latest {
            super::memory::validate_transition(prior, &state)?;
            super::source_apply::validate_transition(prior, &state)?;
            super::writer::mcp::validate_transition(prior, &state)?;
            super::writer::prompt::validate_transition(prior, &state)?;
        }
        if let Some(roles) = &state.role_bindings {
            roles.validate()?;
        }
        super::goal::validate(state.goal.as_ref(), &state.instructions)?;
        state
            .workflow_artifacts
            .validate()
            .map_err(|_| StoreError::Corrupt("workflow artifact graph"))?;
        if let Some(workflow) = &state.workflow {
            workflow.validate_with_artifacts(&state.workflow_artifacts)?;
        }
        for run in &state.runs {
            if let Some(roles) = &run.role_bindings {
                roles.validate()?;
            }
            if let Some(account) = &run.account {
                account.validate()?;
            }
            if let Some(workflow) = &run.workflow {
                workflow.validate_with_artifacts(&state.workflow_artifacts)?;
            }
        }
        if state.session_revision.get() != revision as u64 {
            return Err(StoreError::Corrupt("付随logの世代順序"));
        }
        latest = Some(state);
    }
    let state = latest.ok_or(StoreError::Corrupt("付随状態がありません"))?;
    if marker.session_revision != state.session_revision
        || marker.content_revision > marker.session_revision
    {
        return Err(StoreError::Corrupt("markerと付随状態の世代不一致"));
    }
    Ok(Published { marker, state, raw })
}

pub(super) fn line(value: &impl Serialize) -> StoreResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CommitPredictionM2a {
    pub base_marker_bytes: Vec<u8>,
    pub base_raw_prefix: Vec<u8>,
    pub base_sidecar_prefix: Vec<u8>,
    pub target_marker: Marker,
    pub target_marker_bytes: Vec<u8>,
    pub target_raw_prefix: Vec<u8>,
    pub target_sidecar_prefix: Vec<u8>,
}

/// Read through one FD and recheck its name and complete metadata. This is a
/// read-only prerequisite, never reconciliation or a durability claim.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn read_exact_m2a(dir: &Directory, name: &str, maximum: u64) -> StoreResult<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let mut file = dir.open(name, false, false)?;
    let before = file.metadata()?;
    if before.len() > maximum {
        return Err(StoreError::Corrupt("desktop evidence length"));
    }
    let stamp = |meta: &fs::Metadata| {
        (
            meta.dev(),
            meta.ino(),
            meta.uid(),
            meta.mode(),
            meta.nlink(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    };
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum.checked_add(1).ok_or(StoreError::Overflow)?)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = dir.open(name, false, false)?.metadata()?;
    if bytes.len() as u64 != before.len()
        || stamp(&before) != stamp(&after)
        || stamp(&after) != stamp(&named)
    {
        return Err(StoreError::RecoveryRequired);
    }
    dir.verify()?;
    Ok(bytes)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn predict_commit_m2a(
    dir: &Directory,
    old: &Published,
    next: &Published,
) -> StoreResult<CommitPredictionM2a> {
    dir.require_next_absent_m2a()?;
    let base_marker_bytes = read_exact_m2a(
        dir,
        MARKER,
        super::source_apply::MAX_D27_MARKER_BYTES as u64,
    )?;
    if serde_json::from_slice::<Marker>(&base_marker_bytes)? != old.marker
        || next.raw.len() < old.raw.len()
    {
        return Err(StoreError::RecoveryRequired);
    }
    let base_raw_prefix = read_exact_m2a(dir, RAW, old.marker.raw_offset.get())?;
    let base_sidecar_prefix = read_exact_m2a(dir, SIDECAR, old.marker.sidecar_offset.get())?;
    if base_raw_prefix.len() as u64 != old.marker.raw_offset.get()
        || content_hash(&base_raw_prefix) != old.marker.raw_hash
        || base_sidecar_prefix.len() as u64 != old.marker.sidecar_offset.get()
        || content_hash(&base_sidecar_prefix) != old.marker.sidecar_hash
    {
        return Err(StoreError::RecoveryRequired);
    }
    let prediction = predict_from_prefixes_m2a(
        old,
        next,
        base_marker_bytes.clone(),
        base_raw_prefix,
        base_sidecar_prefix,
    )?;
    if read_exact_m2a(
        dir,
        MARKER,
        super::source_apply::MAX_D27_MARKER_BYTES as u64,
    )? != base_marker_bytes
    {
        return Err(StoreError::RecoveryRequired);
    }
    dir.require_next_absent_m2a()?;
    Ok(prediction)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn predict_from_prefixes_m2a(
    old: &Published,
    next: &Published,
    base_marker_bytes: Vec<u8>,
    base_raw_prefix: Vec<u8>,
    base_sidecar_prefix: Vec<u8>,
) -> StoreResult<CommitPredictionM2a> {
    if serde_json::from_slice::<Marker>(&base_marker_bytes)? != old.marker
        || next.raw.len() < old.raw.len()
        || base_raw_prefix.len() as u64 != old.marker.raw_offset.get()
        || content_hash(&base_raw_prefix) != old.marker.raw_hash
        || base_sidecar_prefix.len() as u64 != old.marker.sidecar_offset.get()
        || content_hash(&base_sidecar_prefix) != old.marker.sidecar_hash
    {
        return Err(StoreError::RecoveryRequired);
    }
    let mut raw = base_raw_prefix.clone();
    for event in &next.raw[old.raw.len()..] {
        raw.extend(line(event)?);
    }
    let mut target_sidecar_prefix = base_sidecar_prefix.clone();
    target_sidecar_prefix.extend(line(&next.state)?);
    let mut target_marker = next.marker.clone();
    target_marker.raw_offset = DecimalU64::new(raw.len() as u64);
    target_marker.raw_hash = content_hash(&raw);
    target_marker.sidecar_offset = DecimalU64::new(target_sidecar_prefix.len() as u64);
    target_marker.sidecar_hash = content_hash(&target_sidecar_prefix);
    let target_marker_bytes = serde_json::to_vec(&target_marker)?;
    if target_marker_bytes.len() > super::source_apply::MAX_D27_MARKER_BYTES {
        return Err(StoreError::Corrupt("desktop target marker length"));
    }
    Ok(CommitPredictionM2a {
        base_marker_bytes,
        base_raw_prefix,
        base_sidecar_prefix,
        target_marker,
        target_marker_bytes,
        target_raw_prefix: raw,
        target_sidecar_prefix,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn require_published_exact_m2a(
    dir: &Directory,
    published: &Published,
    marker: &[u8],
    raw: &[u8],
    sidecar: &[u8],
) -> StoreResult<()> {
    dir.require_next_absent_m2a()?;
    if serde_json::from_slice::<Marker>(marker)? != published.marker
        || raw.len() as u64 != published.marker.raw_offset.get()
        || content_hash(raw) != published.marker.raw_hash
        || sidecar.len() as u64 != published.marker.sidecar_offset.get()
        || content_hash(sidecar) != published.marker.sidecar_hash
        || read_exact_m2a(
            dir,
            MARKER,
            super::source_apply::MAX_D27_MARKER_BYTES as u64,
        )? != marker
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(())
}

/// No unlink or truncation of state.next is permitted on this publication path.
/// Any pre-existing entry is a blocker for the later joined recovery API.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn commit_precommitted_m2a(
    dir: &Directory,
    old: &Published,
    next: &Published,
    prediction: &CommitPredictionM2a,
) -> StoreResult<Marker> {
    if &predict_commit_m2a(dir, old, next)? != prediction {
        return Err(StoreError::RecoveryRequired);
    }
    append(
        dir,
        RAW,
        old.marker.raw_offset,
        &prediction.target_raw_prefix[prediction.base_raw_prefix.len()..],
    )?;
    append(
        dir,
        SIDECAR,
        old.marker.sidecar_offset,
        &prediction.target_sidecar_prefix[prediction.base_sidecar_prefix.len()..],
    )?;
    dir.require_next_absent_m2a()?;
    for (name, expected) in [
        (RAW, &prediction.target_raw_prefix),
        (SIDECAR, &prediction.target_sidecar_prefix),
    ] {
        if read_exact_m2a(dir, name, expected.len() as u64)? != *expected {
            return Err(StoreError::RecoveryRequired);
        }
    }
    if read_exact_m2a(
        dir,
        MARKER,
        super::source_apply::MAX_D27_MARKER_BYTES as u64,
    )? != prediction.base_marker_bytes
    {
        return Err(StoreError::RecoveryRequired);
    }
    let bytes = &prediction.target_marker_bytes;
    let mut file = dir.open("state.next", true, true)?;
    let midpoint = bytes.len().div_ceil(2);
    record_m2a_mutation(0);
    file.write_all(&bytes[..midpoint])?;
    fail("marker_partial")?;
    record_m2a_mutation(0);
    file.write_all(&bytes[midpoint..])?;
    fail("marker_sync")?;
    record_m2a_mutation(1);
    file.sync_all()?;
    file.seek(SeekFrom::Start(0))?;
    let mut readback = Vec::new();
    (&mut file)
        .take(super::source_apply::MAX_D27_MARKER_BYTES as u64 + 1)
        .read_to_end(&mut readback)?;
    same(&file, &dir.open("state.next", false, false)?)?;
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        if metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(StoreError::RecoveryRequired);
        }
    }
    if readback != *bytes
        || read_exact_m2a(
            dir,
            "state.next",
            super::source_apply::MAX_D27_MARKER_BYTES as u64,
        )? != *bytes
    {
        return Err(StoreError::RecoveryRequired);
    }
    fail("marker_rename")?;
    dir.publish_next()?;
    same(&file, &dir.open(MARKER, false, false)?)?;
    if read_exact_m2a(
        dir,
        MARKER,
        super::source_apply::MAX_D27_MARKER_BYTES as u64,
    )? != *bytes
    {
        return Err(StoreError::RecoveryRequired);
    }
    fail("directory_sync")?;
    dir.sync()?;
    for (name, expected) in [
        (RAW, &prediction.target_raw_prefix),
        (SIDECAR, &prediction.target_sidecar_prefix),
        (MARKER, bytes),
    ] {
        if read_exact_m2a(dir, name, expected.len() as u64)? != *expected {
            return Err(StoreError::RecoveryRequired);
        }
    }
    dir.require_next_absent_m2a()?;
    Ok(prediction.target_marker.clone())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateNextClassificationV1 {
    Absent,
    ProperPrefix,
    ExactTarget,
    Foreign,
    Overlong,
    UnsafeInode,
    Raced,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateNextInspectV1 {
    pub classification: StateNextClassificationV1,
    pub identity: Option<crate::apply_intent::IdentityV1>,
    pub uid: Option<u64>,
    pub mode: Option<u32>,
    pub nlink: Option<u64>,
    pub length: Option<u64>,
    pub hash: Option<crate::management_effect::ManagementEffectHashV1>,
    pub bytes: Option<Vec<u8>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Directory {
    fn next_stat_m2a(&self) -> StoreResult<Option<libc::stat>> {
        use std::os::fd::AsRawFd;
        self.verify()?;
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: pinned directory, fixed name and writable stat storage.
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                c"state.next".as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            Ok(Some(unsafe { value.assume_init() }))
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error.into())
            }
        }
    }

    pub(super) fn inspect_next_m2a(
        &self,
        target: Option<&[u8]>,
    ) -> StoreResult<StateNextInspectV1> {
        use StateNextClassificationV1::*;
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        let mut observation = StateNextInspectV1 {
            classification: Absent,
            identity: None,
            uid: None,
            mode: None,
            nlink: None,
            length: None,
            hash: None,
            bytes: None,
        };
        let Some(before) = self.next_stat_m2a()? else {
            return Ok(observation);
        };
        observation.identity = Some(crate::apply_intent::IdentityV1 {
            device: before.st_dev as u64,
            inode: before.st_ino as u64,
        });
        let uid = before.st_uid;
        #[cfg(test)]
        let uid = M2A_NEXT_UID.with(|value| value.get().unwrap_or(uid));
        observation.uid = Some(uid as u64);
        observation.mode = Some(before.st_mode as u32 & 0o777);
        observation.nlink = Some(before.st_nlink as u64);
        observation.length = u64::try_from(before.st_size).ok();
        if before.st_mode & libc::S_IFMT != libc::S_IFREG
            || uid != unsafe { libc::geteuid() }
            || before.st_mode & 0o777 != 0o600
            || before.st_nlink != 1
            || observation.length.is_none()
        {
            observation.classification = UnsafeInode;
            return Ok(observation);
        }
        #[cfg(test)]
        if let Some((start, done)) = M2A_NEXT_RACE.with(|hook| hook.borrow_mut().take()) {
            start.send(()).map_err(|_| StoreError::RecoveryRequired)?;
            done.recv().map_err(|_| StoreError::RecoveryRequired)?;
        }
        let mut file = match self.open("state.next", false, false) {
            Ok(file) => file,
            Err(_) => {
                observation.classification = Raced;
                return Ok(observation);
            }
        };
        let meta = file.metadata()?;
        if meta.dev() != before.st_dev as u64
            || meta.ino() != before.st_ino as u64
            || meta.uid() != before.st_uid
            || meta.mode() & 0o777 != 0o600
            || meta.nlink() != 1
            || meta.len() != before.st_size as u64
        {
            observation.classification = Raced;
            return Ok(observation);
        }
        let length = meta.len();
        let cap = super::source_apply::MAX_D27_MARKER_BYTES as u64;
        let mut hash = Sha256::new();
        let mut bytes = if length <= cap {
            Some(Vec::with_capacity(length as usize))
        } else {
            None
        };
        let mut remaining = length;
        let mut buffer = [0_u8; 16 * 1024];
        while remaining > 0 {
            let count = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| StoreError::Overflow)?;
            let read = file.read(&mut buffer[..count])?;
            if read == 0 {
                observation.classification = Raced;
                return Ok(observation);
            }
            hash.update(&buffer[..read]);
            if let Some(bytes) = &mut bytes {
                bytes.extend_from_slice(&buffer[..read]);
            }
            remaining -= read as u64;
        }
        let after = file.metadata()?;
        let named = self.next_stat_m2a()?;
        if after.dev() != meta.dev()
            || after.ino() != meta.ino()
            || after.uid() != meta.uid()
            || after.mode() != meta.mode()
            || after.nlink() != meta.nlink()
            || after.len() != length
            || after.mtime() != meta.mtime()
            || after.mtime_nsec() != meta.mtime_nsec()
            || after.ctime() != meta.ctime()
            || after.ctime_nsec() != meta.ctime_nsec()
            || named.is_none_or(|s| {
                s.st_dev as u64 != after.dev()
                    || s.st_ino as u64 != after.ino()
                    || s.st_uid != after.uid()
                    || s.st_mode as u32 != after.mode()
                    || s.st_nlink as u64 != after.nlink()
                    || s.st_size as u64 != length
            })
        {
            observation.classification = Raced;
            return Ok(observation);
        }
        observation.hash = Some(
            crate::management_effect::ManagementEffectHashV1::from_hex(&format!(
                "{:x}",
                hash.finalize()
            ))
            .map_err(|_| StoreError::Corrupt("state.next digest"))?,
        );
        observation.bytes = bytes;
        observation.classification = if length > cap {
            Overlong
        } else if let Some(target) = target {
            let bytes = observation
                .bytes
                .as_deref()
                .ok_or(StoreError::RecoveryRequired)?;
            if bytes.len() > target.len() {
                Overlong
            } else if bytes == target {
                ExactTarget
            } else if target.starts_with(bytes) {
                ProperPrefix
            } else {
                Foreign
            }
        } else {
            Foreign
        };
        self.verify()?;
        Ok(observation)
    }

    pub(super) fn names_m2a(&self) -> StoreResult<Vec<String>> {
        use std::os::fd::AsRawFd;
        self.verify()?;
        // SAFETY: opening "." obtains an independent directory stream offset.
        let fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(std::io::Error::last_os_error().into());
        }
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = Stream(stream);
        let mut names = Vec::new();
        loop {
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error().unwrap_or(0) != 0 {
                    return Err(error.into());
                }
                break;
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_str()
                .map_err(|_| StoreError::Corrupt("desktop entry encoding"))?;
            if name != "." && name != ".." {
                names.push(name.to_owned());
            }
            if names.len() > 1024 {
                return Err(StoreError::Capacity("desktop inspect entries"));
            }
        }
        names.sort();
        self.verify()?;
        Ok(names)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct D27ReadFileV1 {
    pub relative_name: String,
    pub length: u64,
    pub hash: crate::management_effect::ManagementEffectHashV1,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub(super) struct PublishedInspectionM2a {
    pub published: Published,
    pub marker_bytes: Vec<u8>,
    pub raw_bytes: Vec<u8>,
    pub sidecar_bytes: Vec<u8>,
    pub state_next: StateNextInspectV1,
    pub files: Vec<D27ReadFileV1>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn inspect_published_m2a(
    dir: &Directory,
    project: &ProjectId,
    session: &SessionId,
) -> StoreResult<PublishedInspectionM2a> {
    use crate::management_effect::ManagementEffectHashV1 as Hash;
    const MAX_INSPECT_FILE_BYTES: u64 = 256 * 1024 * 1024;
    let names = dir.names_m2a()?;
    let marker_bytes = read_exact_m2a(
        dir,
        MARKER,
        super::source_apply::MAX_D27_MARKER_BYTES as u64,
    )?;
    let raw_bytes = read_exact_m2a(dir, RAW, MAX_INSPECT_FILE_BYTES)?;
    let sidecar_bytes = read_exact_m2a(dir, SIDECAR, MAX_INSPECT_FILE_BYTES)?;
    let published = load(dir, project, session)?;
    let marker = &published.marker;
    if serde_json::from_slice::<Marker>(&marker_bytes)? != *marker
        || marker.raw_offset.get() > raw_bytes.len() as u64
        || marker.sidecar_offset.get() > sidecar_bytes.len() as u64
        || content_hash(&raw_bytes[..marker.raw_offset.get() as usize]) != marker.raw_hash
        || content_hash(&sidecar_bytes[..marker.sidecar_offset.get() as usize])
            != marker.sidecar_hash
    {
        return Err(StoreError::RecoveryRequired);
    }
    let state_next = dir.inspect_next_m2a(None)?;
    let mut files = Vec::new();
    for name in &names {
        if name == "state.next" {
            if let (Some(length), Some(hash)) = (state_next.length, state_next.hash) {
                files.push(D27ReadFileV1 {
                    relative_name: name.clone(),
                    length,
                    hash,
                });
            }
            continue;
        }
        let bytes = read_exact_m2a(dir, name, MAX_INSPECT_FILE_BYTES)?;
        files.push(D27ReadFileV1 {
            relative_name: name.clone(),
            length: bytes.len() as u64,
            hash: Hash::digest(&bytes),
        });
    }
    if dir.names_m2a()? != names
        || dir.inspect_next_m2a(None)? != state_next
        || read_exact_m2a(
            dir,
            MARKER,
            super::source_apply::MAX_D27_MARKER_BYTES as u64,
        )? != marker_bytes
        || read_exact_m2a(dir, RAW, MAX_INSPECT_FILE_BYTES)? != raw_bytes
        || read_exact_m2a(dir, SIDECAR, MAX_INSPECT_FILE_BYTES)? != sidecar_bytes
    {
        return Err(StoreError::RecoveryRequired);
    }
    Ok(PublishedInspectionM2a {
        published,
        marker_bytes,
        raw_bytes,
        sidecar_bytes,
        state_next,
        files,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn historical_published_m2a(
    observed: &PublishedInspectionM2a,
    revision: u64,
) -> StoreResult<(Published, Vec<u8>, Vec<u8>, Vec<u8>)> {
    let row_count = usize::try_from(revision.checked_add(1).ok_or(StoreError::Overflow)?)
        .map_err(|_| StoreError::Overflow)?;
    let mut side = Vec::new();
    let mut count = 0;
    for row in observed
        .sidecar_bytes
        .split_inclusive(|b| *b == b'\n')
        .take(row_count)
    {
        side.extend_from_slice(row);
        count += 1;
    }
    if count != row_count || side.last() != Some(&b'\n') {
        return Err(StoreError::RecoveryRequired);
    }
    let raw = observed
        .raw_bytes
        .get(..observed.published.marker.raw_offset.get() as usize)
        .ok_or(StoreError::RecoveryRequired)?
        .to_vec();
    let mut marker = observed.published.marker.clone();
    marker.session_revision = DecimalU64::new(revision);
    marker.sidecar_offset = DecimalU64::new(side.len() as u64);
    marker.sidecar_hash = content_hash(&side);
    let marker_bytes = if marker == observed.published.marker {
        observed.marker_bytes.clone()
    } else {
        serde_json::to_vec(&marker)?
    };
    Ok((
        decode_prefixes_m2a(marker, &raw, &side)?,
        marker_bytes,
        raw,
        side,
    ))
}

/// This primitive accepts only the private prediction rebuilt by the ledger
/// owner and the complete observation retained by its joined capability.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn recover_published_m2a(
    dir: &Directory,
    prediction: &CommitPredictionM2a,
    marker_observed: &[u8],
    raw_observed: &[u8],
    side_observed: &[u8],
    next_observed: &StateNextInspectV1,
) -> StoreResult<bool> {
    use StateNextClassificationV1::*;
    let bytes = &prediction.target_marker_bytes;
    if read_exact_m2a(
        dir,
        MARKER,
        super::source_apply::MAX_D27_MARKER_BYTES as u64,
    )? != marker_observed
        || read_exact_m2a(dir, RAW, raw_observed.len() as u64)? != raw_observed
        || read_exact_m2a(dir, SIDECAR, side_observed.len() as u64)? != side_observed
        || dir.inspect_next_m2a(Some(bytes))? != *next_observed
    {
        return Err(StoreError::RecoveryRequired);
    }
    let base_logs = raw_observed == prediction.base_raw_prefix
        && side_observed == prediction.base_sidecar_prefix;
    let target_logs = raw_observed == prediction.target_raw_prefix
        && side_observed == prediction.target_sidecar_prefix;
    if marker_observed == prediction.base_marker_bytes
        && base_logs
        && next_observed.classification == Absent
    {
        return Ok(false);
    }
    if marker_observed == prediction.target_marker_bytes
        && target_logs
        && next_observed.classification == Absent
    {
        sync_published(dir)?;
        require_target_readback_m2a(dir, prediction)?;
        return Ok(true);
    }
    if marker_observed != prediction.base_marker_bytes
        || !target_logs
        || !matches!(
            next_observed.classification,
            Absent | ProperPrefix | ExactTarget
        )
    {
        return Err(StoreError::RecoveryRequired);
    }
    if next_observed.classification == ProperPrefix {
        let pinned = dir.open("state.next", true, false)?;
        if dir.inspect_next_m2a(Some(bytes))? != *next_observed {
            return Err(StoreError::RecoveryRequired);
        }
        same(&pinned, &dir.open("state.next", false, false)?)?;
        dir.unlink_file("state.next")?;
        fail("m2a_prefix_unlinked")?;
        dir.sync()?;
        fail("m2a_prefix_cleanup_sync")?;
        dir.require_next_absent_m2a()?;
    }
    let mut file = if next_observed.classification == ExactTarget {
        let file = dir.open("state.next", true, false)?;
        if dir.inspect_next_m2a(Some(bytes))? != *next_observed {
            return Err(StoreError::RecoveryRequired);
        }
        file
    } else {
        dir.require_next_absent_m2a()?;
        let mut file = dir.open("state.next", true, true)?;
        fail("m2a_next_created")?;
        let midpoint = bytes.len().div_ceil(2);
        record_m2a_mutation(0);
        file.write_all(&bytes[..midpoint])?;
        fail("marker_partial")?;
        record_m2a_mutation(0);
        file.write_all(&bytes[midpoint..])?;
        file
    };
    fail("marker_sync")?;
    record_m2a_mutation(1);
    file.sync_all()?;
    fail("m2a_next_synced")?;
    file.seek(SeekFrom::Start(0))?;
    let mut readback = Vec::new();
    (&mut file)
        .take(super::source_apply::MAX_D27_MARKER_BYTES as u64 + 1)
        .read_to_end(&mut readback)?;
    same(&file, &dir.open("state.next", false, false)?)?;
    if readback != *bytes
        || dir.inspect_next_m2a(Some(bytes))?.classification != ExactTarget
        || read_exact_m2a(dir, RAW, raw_observed.len() as u64)? != raw_observed
        || read_exact_m2a(dir, SIDECAR, side_observed.len() as u64)? != side_observed
        || read_exact_m2a(
            dir,
            MARKER,
            super::source_apply::MAX_D27_MARKER_BYTES as u64,
        )? != marker_observed
    {
        return Err(StoreError::RecoveryRequired);
    }
    fail("marker_rename")?;
    dir.publish_next()?;
    same(&file, &dir.open(MARKER, false, false)?)?;
    fail("directory_sync")?;
    dir.sync()?;
    fail("m2a_directory_synced")?;
    require_target_readback_m2a(dir, prediction)?;
    Ok(true)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn require_target_readback_m2a(
    dir: &Directory,
    prediction: &CommitPredictionM2a,
) -> StoreResult<()> {
    for (name, expected) in [
        (MARKER, &prediction.target_marker_bytes),
        (RAW, &prediction.target_raw_prefix),
        (SIDECAR, &prediction.target_sidecar_prefix),
    ] {
        if read_exact_m2a(dir, name, expected.len() as u64)? != *expected {
            return Err(StoreError::RecoveryRequired);
        }
    }
    dir.require_next_absent_m2a()
}

fn append(dir: &Directory, name: &str, old_offset: DecimalU64, bytes: &[u8]) -> StoreResult<()> {
    let mut file = dir.open(name, true, false)?;
    file.set_len(old_offset.get())?;
    file.seek(SeekFrom::Start(old_offset.get()))?;
    let midpoint = bytes.len().div_ceil(2);
    record_m2a_mutation(0);
    file.write_all(&bytes[..midpoint])?;
    fail(if name == RAW {
        "raw_partial"
    } else {
        "sidecar_partial"
    })?;
    record_m2a_mutation(0);
    file.write_all(&bytes[midpoint..])?;
    fail(if name == RAW {
        "raw_sync"
    } else {
        "sidecar_sync"
    })?;
    record_m2a_mutation(1);
    file.sync_all()?;
    Ok(())
}

pub(super) fn publish_marker(dir: &Directory, marker: &Marker) -> StoreResult<()> {
    dir.remove_next()?;
    let bytes = serde_json::to_vec(marker)?;
    let mut file = dir.open("state.next", true, true)?;
    let midpoint = bytes.len().div_ceil(2);
    record_m2a_mutation(0);
    file.write_all(&bytes[..midpoint])?;
    fail("marker_partial")?;
    record_m2a_mutation(0);
    file.write_all(&bytes[midpoint..])?;
    fail("marker_sync")?;
    record_m2a_mutation(1);
    file.sync_all()?;
    drop(file);
    fail("marker_rename")?;
    dir.publish_next()?;
    fail("directory_sync")?;
    dir.sync()
}

pub(super) fn commit(dir: &Directory, old: &Published, next: &Published) -> StoreResult<Marker> {
    if read_marker(dir)? != old.marker {
        return Err(StoreError::Corrupt("公開markerがwriter外で変更されました"));
    }
    let mut raw = prefix(dir, RAW, old.marker.raw_offset, &old.marker.raw_hash)?;
    let mut sidecar = prefix(
        dir,
        SIDECAR,
        old.marker.sidecar_offset,
        &old.marker.sidecar_hash,
    )?;
    let mut raw_delta = Vec::new();
    for event in &next.raw[old.raw.len()..] {
        raw_delta.extend(line(event)?);
    }
    let side_delta = line(&next.state)?;
    append(dir, RAW, old.marker.raw_offset, &raw_delta)?;
    append(dir, SIDECAR, old.marker.sidecar_offset, &side_delta)?;
    raw.extend(raw_delta);
    sidecar.extend(side_delta);
    let mut marker = next.marker.clone();
    marker.raw_offset = DecimalU64::new(raw.len() as u64);
    marker.raw_hash = content_hash(&raw);
    marker.sidecar_offset = DecimalU64::new(sidecar.len() as u64);
    marker.sidecar_hash = content_hash(&sidecar);
    publish_marker(dir, &marker)?;
    Ok(marker)
}

pub(super) fn sync_published(dir: &Directory) -> StoreResult<()> {
    for name in [RAW, SIDECAR, MARKER] {
        record_m2a_mutation(1);
        dir.open(name, false, false)?.sync_all()?;
    }
    fail("recovery_sync")?;
    dir.sync()
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

pub(super) fn fail(_point: &str) -> StoreResult<()> {
    #[cfg(test)]
    if FAIL.with(|f| f.get() == Some(_point)) {
        return Err(StoreError::Io(std::io::Error::other("注入した保存障害")));
    }
    Ok(())
}

#[cfg(unix)]
fn open_error() -> StoreError {
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ELOOP) {
        StoreError::Corrupt("保存対象のsymlinkを拒否しました")
    } else {
        StoreError::Io(error)
    }
}
