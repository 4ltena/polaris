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
    #[cfg(all(test, unix))]
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
        if create && unsafe { libc::mkdirat(self.file.as_raw_fd(), component.as_ptr(), 0o700) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
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
    fn remove_next(&self) -> StoreResult<()> {
        use std::os::fd::AsRawFd;
        match self.open("state.next", false, false) {
            Ok(_) => (),
            Err(StoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), c"state.next".as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    fn remove_next(&self) -> StoreResult<()> {
        Err(StoreError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    fn publish_next(&self) -> StoreResult<()> {
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
    #[cfg(not(unix))]
    fn publish_next(&self) -> StoreResult<()> {
        Err(StoreError::UnsupportedPlatform)
    }
    pub(super) fn sync(&self) -> StoreResult<()> {
        self.verify()?;
        self.file.sync_all()?;
        self.verify()
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
    for (revision, line) in side_bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        let state: Sidecar = serde_json::from_slice(line)?;
        super::children::validate(&state)?;
        super::source_apply::validate(&state)?;
        if let Some(prior) = &latest {
            super::source_apply::validate_transition(prior, &state)?;
        }
        if let Some(roles) = &state.role_bindings {
            roles.validate()?;
        }
        if let Some(workflow) = &state.workflow {
            workflow.validate()?;
        }
        for run in &state.runs {
            if let Some(roles) = &run.role_bindings {
                roles.validate()?;
            }
            if let Some(workflow) = &run.workflow {
                workflow.validate()?;
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

fn append(dir: &Directory, name: &str, old_offset: DecimalU64, bytes: &[u8]) -> StoreResult<()> {
    let mut file = dir.open(name, true, false)?;
    file.set_len(old_offset.get())?;
    file.seek(SeekFrom::Start(old_offset.get()))?;
    let midpoint = bytes.len().div_ceil(2);
    file.write_all(&bytes[..midpoint])?;
    fail(if name == RAW {
        "raw_partial"
    } else {
        "sidecar_partial"
    })?;
    file.write_all(&bytes[midpoint..])?;
    fail(if name == RAW {
        "raw_sync"
    } else {
        "sidecar_sync"
    })?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn publish_marker(dir: &Directory, marker: &Marker) -> StoreResult<()> {
    dir.remove_next()?;
    let bytes = serde_json::to_vec(marker)?;
    let mut file = dir.open("state.next", true, true)?;
    let midpoint = bytes.len().div_ceil(2);
    file.write_all(&bytes[..midpoint])?;
    fail("marker_partial")?;
    file.write_all(&bytes[midpoint..])?;
    fail("marker_sync")?;
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
