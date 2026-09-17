//! M7b-1: who takes part in a shared project, what is shared, and where the
//! synchronization repository lives. No network, no Git and no copying here:
//! this module only decides identities, paths and the shared file set, so the
//! rules can be checked without a remote.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A share link carries where to exchange, which project, and an invitation
/// with an end. It never carries a credential: the remote grants access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareLink {
    /// The synchronization remote the participants already have access to.
    pub remote: String,
    pub share_id: String,
    pub invitation_id: String,
    pub expires_unix_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRefusal {
    /// A scheme Polaris will not hand to Git, or an embedded credential.
    Remote,
    Identifier,
    Expired,
}

const ALLOWED_SCHEMES: &[&str] = &["ssh://", "https://", "git@", "file://"];

impl ShareLink {
    /// Checked before the link is stored or used. An expired invitation stops
    /// a new participant; it never removes one that already joined.
    pub fn accept(&self, now_unix_seconds: u64) -> Result<(), LinkRefusal> {
        if !ALLOWED_SCHEMES
            .iter()
            .any(|scheme| self.remote.starts_with(scheme))
            || self.remote.len() > 512
            || carries_credential(&self.remote)
        {
            // `user:password@host` and unknown schemes are both refused; the
            // remote must be reachable with the credentials Git already has.
            return Err(LinkRefusal::Remote);
        }
        if !valid_id(&self.share_id) || !valid_id(&self.invitation_id) {
            return Err(LinkRefusal::Identifier);
        }
        if self.expires_unix_seconds <= now_unix_seconds {
            return Err(LinkRefusal::Expired);
        }
        Ok(())
    }
}

/// A password in the authority, as in `https://user:secret@host/x.git`. A
/// bare user (`ssh://git@host`) is how Git is normally addressed and is kept.
fn carries_credential(remote: &str) -> bool {
    let authority = remote
        .split_once("://")
        .map_or(remote, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    authority
        .split_once('@')
        .is_some_and(|(user, _)| user.contains(':'))
}

fn valid_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where one project's synchronization state lives. Everything is under the
/// Polaris state directory, never inside the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPaths {
    /// The synchronization repository; never the project's own `.git`.
    pub git_dir: PathBuf,
    /// The copy Git works on. The project directory is never a work tree.
    pub work_tree: PathBuf,
    pub index: PathBuf,
}

impl SyncPaths {
    pub fn under(state_dir: &Path) -> Self {
        let root = state_dir.join("sync");
        Self {
            git_dir: root.join("repository"),
            work_tree: root.join("staging"),
            index: root.join("index"),
        }
    }

    /// True when the path is inside this project's own directory, which would
    /// mean Git could reach the project's files directly.
    pub fn overlaps(&self, project_root: &Path) -> bool {
        [&self.git_dir, &self.work_tree, &self.index]
            .iter()
            .any(|path| path.starts_with(project_root))
    }
}

/// What the participants agreed to share. Both sides must hold the same
/// rules, so it carries a version and is compared as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareManifest {
    pub version: u32,
    /// Extra names to exclude, on top of the ones that are always excluded.
    pub excluded: Vec<String>,
}

/// Never shared, whatever the manifest says: the project's own repository,
/// Polaris state, credentials and build output. Names a project legitimately
/// uses for its own sources (`dist`, `build`, `out`) are left to the
/// manifest instead of being taken away from every project.
const ALWAYS_EXCLUDED: &[&str] = &[
    ".git",
    ".polaris",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    ".DS_Store",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    ".parcel-cache",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "coverage",
];

/// `.env` is not one name. A project keeps its keys in `.env.local`,
/// `.env.production.local` or `.envrc`, so every name that starts that way is
/// treated as a credential and never leaves this device.
fn always_excluded(name: &str) -> bool {
    ALWAYS_EXCLUDED.contains(&name) || name.starts_with(".env")
}

impl Default for ShareManifest {
    fn default() -> Self {
        Self {
            version: 1,
            excluded: Vec::new(),
        }
    }
}

impl ShareManifest {
    /// A stable digest both sides compare before exchanging anything.
    pub fn digest(&self) -> String {
        let mut excluded = self.excluded.clone();
        excluded.sort();
        excluded.dedup();
        let body = format!("{}\n{}", self.version, excluded.join("\n"));
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(body.as_bytes()))
    }

    /// Whether one path inside the project is shared. `relative` must already
    /// be relative to the project root.
    pub fn shares(&self, relative: &Path) -> bool {
        if relative.as_os_str().is_empty() {
            return false;
        }
        relative.components().all(|component| {
            let Some(name) = component.as_os_str().to_str() else {
                // A name that is not UTF-8 cannot be compared across systems.
                return false;
            };
            if name == "." || name == ".." || name.is_empty() {
                return false;
            }
            !always_excluded(name) && !self.excluded.iter().any(|excluded| excluded == name)
        })
    }
}

#[cfg(test)]
mod tests;
