//! M7b-5: what this device remembers about a shared project. One small file
//! per project, next to the rest of its state. It holds no credential: the
//! remote it names is reached with the credentials Git already has.
use super::share::{ShareLink, ShareManifest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub const FILE: &str = "participation.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participation {
    pub share: ShareLink,
    pub manifest: ShareManifest,
    pub joined_unix_seconds: u64,
    /// The last commit this device agreed with the others on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreed_commit: Option<String>,
    /// The agreed commit this device has already put into the project. While
    /// it differs from `agreed_commit`, the project does not hold what the
    /// others sent, so sending from the project would take their work back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_commit: Option<String>,
    /// Paths waiting for a person because both versions were kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
}

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("sync").join(FILE)
}

/// What this device joined, if anything.
pub fn read(state_dir: &Path) -> Option<Participation> {
    let body = fs::read(path(state_dir)).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Records the join. An invitation that has run out is refused here, so an
/// old link cannot be used to join later.
pub fn join(
    state_dir: &Path,
    share: ShareLink,
    manifest: ShareManifest,
    now_unix_seconds: u64,
) -> io::Result<Participation> {
    share
        .accept(now_unix_seconds)
        .map_err(|_| io::Error::other("この共有リンクは使えません"))?;
    if let Some(held) = read(state_dir)
        && held.share.share_id != share.share_id
    {
        // One project belongs to one share; joining a second would mix two
        // histories into the same folder.
        return Err(io::Error::other("このprojectは別の共有に参加しています"));
    }
    let participation = Participation {
        share,
        manifest,
        joined_unix_seconds: now_unix_seconds,
        agreed_commit: None,
        applied_commit: None,
        unresolved: Vec::new(),
    };
    write(state_dir, &participation)?;
    Ok(participation)
}

pub fn write(state_dir: &Path, participation: &Participation) -> io::Result<()> {
    let file = path(state_dir);
    fs::create_dir_all(file.parent().expect("a parent"))?;
    let body = serde_json::to_vec_pretty(participation).map_err(io::Error::other)?;
    let temporary = file.with_extension("tmp");
    fs::write(&temporary, body)?;
    fs::rename(&temporary, file)
}

/// Stops participating on this device. The shared history stays where it is,
/// and the other participants are unaffected.
pub fn leave(state_dir: &Path) -> io::Result<bool> {
    match fs::remove_file(path(state_dir)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
