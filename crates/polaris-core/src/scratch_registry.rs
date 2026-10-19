//! M6-5: the private copies Polaris created, recorded so that a later start
//! can remove the ones a killed process left behind. Only a path this
//! registry recorded is ever considered, every candidate is re-checked
//! immediately before removal, and anything that does not match exactly is
//! kept. This performs no general search of the temporary directory.
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The registry lives beside the other per-installation state.
pub const FILE: &str = "scratch.json";
/// The name every recorded copy must still carry.
#[cfg(unix)]
const PREFIX: &str = "polaris-workspace-";
#[cfg(unix)]
const MAX_ENTRIES: usize = 64;
/// A copy younger than this is assumed to belong to a live preparation even
/// when its process cannot be seen.
pub const RETENTION_SECONDS: u64 = 6 * 60 * 60;

static REGISTRY: OnceLock<PathBuf> = OnceLock::new();

/// Installed once by the trusted owner at startup. Without it nothing is
/// recorded and nothing is ever removed.
pub fn install(state_root: &Path) {
    let _ = REGISTRY.set(state_root.join(FILE));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: PathBuf,
    pub pid: u32,
    pub created_unix_seconds: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Registry {
    pub entries: Vec<Entry>,
    pub last_sweep_unix_seconds: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub removed: Vec<PathBuf>,
    /// Still in use, or too young to judge.
    pub kept: usize,
    /// Recorded but no longer recognizable; left untouched on purpose.
    pub refused: Vec<PathBuf>,
}

#[cfg(unix)]
fn read(path: &Path) -> Registry {
    fs::read(path)
        .ok()
        .and_then(|body| serde_json::from_slice(&body).ok())
        .unwrap_or_default()
}

#[cfg(unix)]
fn write(path: &Path, registry: &Registry) -> io::Result<()> {
    let body = serde_json::to_vec(registry).map_err(io::Error::other)?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, body)?;
    fs::rename(&temporary, path)
}

/// Records one private copy. A failure to record is not a reason to fail the
/// preparation: the copy is still removed when its owner exits normally.
#[cfg(unix)]
pub fn record(path: &Path, now: u64) {
    let Some(registry_path) = REGISTRY.get() else {
        return;
    };
    let mut registry = read(registry_path);
    registry.entries.retain(|entry| entry.path.exists());
    if registry.entries.len() >= MAX_ENTRIES {
        registry.entries.remove(0);
    }
    registry.entries.push(Entry {
        path: path.to_owned(),
        pid: std::process::id(),
        created_unix_seconds: now,
    });
    let _ = write(registry_path, &registry);
}

/// Removes the recorded copies that no live process can still be using.
/// `alive` reports whether a process id is still running.
#[cfg(unix)]
pub fn sweep(now: u64, retention_seconds: u64, alive: impl Fn(u32) -> bool) -> io::Result<Swept> {
    let Some(registry_path) = REGISTRY.get() else {
        return Ok(Swept::default());
    };
    let mut registry = read(registry_path);
    let mut swept = Swept::default();
    let mut kept = Vec::new();
    for entry in std::mem::take(&mut registry.entries) {
        if !entry.path.exists() {
            continue; // Its owner removed it; the record is simply stale.
        }
        let young = now.saturating_sub(entry.created_unix_seconds) < retention_seconds;
        if young || alive(entry.pid) {
            swept.kept += 1;
            kept.push(entry);
            continue;
        }
        match removable(&entry.path) {
            Ok(()) => match fs::remove_dir_all(&entry.path) {
                Ok(()) => swept.removed.push(entry.path),
                Err(_) => {
                    swept.refused.push(entry.path.clone());
                    kept.push(entry);
                }
            },
            Err(()) => {
                swept.refused.push(entry.path.clone());
                kept.push(entry);
            }
        }
    }
    registry.entries = kept;
    registry.last_sweep_unix_seconds = now;
    write(registry_path, &registry)?;
    Ok(swept)
}

#[cfg(not(unix))]
pub fn sweep(
    _now: u64,
    _retention_seconds: u64,
    _alive: impl Fn(u32) -> bool,
) -> io::Result<Swept> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "scratch registry cleanup is unavailable on this platform",
    ))
}

/// Every condition the copy had when it was created must still hold. A path
/// that fails any of them is left alone, not repaired.
#[cfg(unix)]
fn removable(path: &Path) -> Result<(), ()> {
    let name = path.file_name().and_then(|name| name.to_str()).ok_or(())?;
    if !name.starts_with(PREFIX) {
        return Err(());
    }
    let temporary = fs::canonicalize(std::env::temp_dir()).map_err(|_| ())?;
    if path.parent() != Some(temporary.as_path()) {
        return Err(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.is_dir()
        || metadata.uid() != nix_uid()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(());
    }
    Ok(())
}

#[cfg(unix)]
fn nix_uid() -> u32 {
    // SAFETY: getuid is always successful and has no side effects.
    unsafe { libc::getuid() }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, windows))]
#[path = "scratch_registry/windows_tests.rs"]
mod windows_tests;
