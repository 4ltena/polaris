//! M7b-1: who takes part in a shared project, what is shared, and where the
//! synchronization repository lives. No network, no Git and no copying here:
//! this module only decides identities, paths and the shared file set, so the
//! rules can be checked without a remote.
use super::remote_intent::SyncDigestV1;
pub use polaris_auth::git_credentials::CanonicalHttpsRemoteV1;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// A share link carries where to exchange, which project, and an invitation
/// with an end. It never carries a credential: the remote grants access.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The display identity never retains the transport locator, including when
/// an older participation names a remote that production no longer accepts.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoteDisplayV1 {
    pub scheme: SyncRemoteSchemeV1,
    pub host_hash: SyncDigestV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRemoteSchemeV1 {
    Https,
    Ssh,
    File,
    Unsupported,
}

impl SyncRemoteSchemeV1 {
    fn label(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Ssh => "ssh",
            Self::File => "file",
            Self::Unsupported => "unsupported",
        }
    }
}

impl fmt::Display for SyncRemoteDisplayV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}://[host:{}]",
            self.scheme.label(),
            &self.host_hash.as_hex()[..12]
        )
    }
}

impl fmt::Debug for SyncRemoteDisplayV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Debug for ShareLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShareLink")
            .field("remote", &self.remote_display())
            .field("share_id", &self.share_id)
            .field("invitation_id", &self.invitation_id)
            .field("expires_unix_seconds", &self.expires_unix_seconds)
            .finish()
    }
}

impl ShareLink {
    /// Checked before the link is stored or used. An expired invitation stops
    /// a new participant; it never removes one that already joined.
    pub fn accept(&self, now_unix_seconds: u64) -> Result<(), LinkRefusal> {
        self.validate_legacy()?;
        if self.expires_unix_seconds <= now_unix_seconds {
            return Err(LinkRefusal::Expired);
        }
        Ok(())
    }

    /// Historical participation is inspected without renewing its invitation
    /// or silently rewriting its locator. Credential-bearing URLs are never
    /// eligible for the legacy rebind exception.
    pub(crate) fn validate_legacy(&self) -> Result<(), LinkRefusal> {
        if !ALLOWED_SCHEMES
            .iter()
            .any(|scheme| self.remote.starts_with(scheme))
            || self.remote.len() > 512
            || self
                .remote
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || self.remote.contains(['?', '#', '\\'])
            || carries_credential(&self.remote)
            || (self.remote.starts_with("https://") && remote_authority(&self.remote).contains('@'))
        {
            return Err(LinkRefusal::Remote);
        }
        if !valid_legacy_locator(&self.remote) {
            return Err(LinkRefusal::Remote);
        }
        if !valid_id(&self.share_id)
            || !valid_id(&self.invitation_id)
            || self.expires_unix_seconds == 0
        {
            return Err(LinkRefusal::Identifier);
        }
        Ok(())
    }

    /// The production boundary is deliberately separate from historical
    /// inspection. It never accepts SSH, scp-style or file transports.
    pub fn production_remote(&self) -> Result<CanonicalHttpsRemoteV1, LinkRefusal> {
        CanonicalHttpsRemoteV1::parse(&self.remote).map_err(|_| LinkRefusal::Remote)
    }

    pub fn accept_production(
        &self,
        now_unix_seconds: u64,
    ) -> Result<CanonicalHttpsRemoteV1, LinkRefusal> {
        self.accept(now_unix_seconds)?;
        self.production_remote()
    }

    pub fn remote_display(&self) -> SyncRemoteDisplayV1 {
        let scheme = if self.remote.starts_with("https://") {
            SyncRemoteSchemeV1::Https
        } else if self.remote.starts_with("ssh://") || self.remote.starts_with("git@") {
            SyncRemoteSchemeV1::Ssh
        } else if self.remote.starts_with("file://") {
            SyncRemoteSchemeV1::File
        } else {
            SyncRemoteSchemeV1::Unsupported
        };
        let authority = remote_authority(&self.remote);
        let host = authority.rsplit('@').next().unwrap_or_default();
        SyncRemoteDisplayV1 {
            scheme,
            host_hash: SyncDigestV1::digest(host.as_bytes()),
        }
    }
}

fn remote_authority(remote: &str) -> &str {
    remote
        .split_once("://")
        .map_or(remote, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default()
}

fn valid_legacy_locator(remote: &str) -> bool {
    if let Some(rest) = remote.strip_prefix("git@") {
        return rest
            .split_once(':')
            .is_some_and(|(host, path)| valid_legacy_host(host) && !path.is_empty());
    }
    let Some((scheme, rest)) = remote.split_once("://") else {
        return false;
    };
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, ""), |(host, path)| (host, path));
    if scheme == "file" {
        return rest.contains('/') && (authority.is_empty() || valid_legacy_host(authority));
    }
    let authority = if scheme == "ssh" {
        match authority.split_once('@') {
            Some((user, host))
                if !user.is_empty()
                    && user.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    }) =>
            {
                host
            }
            Some(_) => return false,
            None => authority,
        }
    } else {
        authority
    };
    if !valid_legacy_host(authority) {
        return false;
    }
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && (!bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit()))
        {
            return false;
        }
    }
    true
}

fn valid_legacy_host(authority: &str) -> bool {
    let (host, port) = if authority.starts_with('[') {
        let Some(end) = authority.find(']') else {
            return false;
        };
        if authority[1..end].parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        let suffix = &authority[end + 1..];
        if suffix.is_empty() {
            return true;
        }
        let Some(port) = suffix.strip_prefix(':') else {
            return false;
        };
        (&authority[..=end], Some(port))
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        if host.is_empty()
            || !host.is_ascii()
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return false;
        }
        (host, port)
    };
    !host.is_empty()
        && port.is_none_or(|port| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|port| port != 0)
        })
}

/// A password in the authority, as in `https://user:secret@host/x.git`. A
/// bare user (`ssh://git@host`) is how Git is normally addressed and is kept.
fn carries_credential(remote: &str) -> bool {
    let authority = remote_authority(remote);
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
