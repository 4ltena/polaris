//! Fixed environment profiles shared by the CLI, TUI and typed host tool.
//! A profile describes resources; it never grants permission to fetch or publish.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub mod acquisition;
pub mod adapter;
pub mod archive;
pub mod bundled;
pub mod catalog;
pub mod definition;
pub mod invocation;
pub mod journal;
pub mod plan;
pub mod process;
pub mod profile;
pub mod publication;
pub mod recipes;
pub mod request;
pub mod runner;
pub mod transfer;
pub mod version;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("unsupported environment profile or platform")]
    Unsupported,
    #[error("environment profile contains an invalid artifact or dependency closure")]
    InvalidProfile,
    #[error("environment request does not match the approved plan")]
    StalePlan,
    #[error("environment operation requires recovery; no effects were repeated")]
    RecoveryRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub version: String,
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
}

impl Artifact {
    fn validate(&self) -> Result<(), Error> {
        self.validate_for_origins(
            &[
                "https://registry.npmjs.org".into(),
                "https://nodejs.org".into(),
            ],
            128 * 1024 * 1024,
        )
    }

    pub(crate) fn validate_for_origins(
        &self,
        origins: &[String],
        maximum: u64,
    ) -> Result<(), Error> {
        let url = url::Url::parse(&self.url).map_err(|_| Error::InvalidProfile)?;
        if self.name.is_empty()
            || self.version.is_empty()
            || self.name.len() > 256
            || self.version.len() > 64
            || self.bytes == 0
            || self.bytes > maximum
            || !is_digest(&self.sha256)
            || url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port().is_some_and(|port| port != 443)
            || !origins.contains(&url.origin().ascii_serialization())
            || self.url.contains('\\')
            || self
                .url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::InvalidProfile);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub next: String,
    pub node: Artifact,
    pub packages: Vec<Artifact>,
    /// SHA-256 of the complete, bundled npm v3 lockfile.
    pub lock_sha256: String,
    pub manifest: String,
    pub lock: String,
    pub npm_version: String,
    pub execution: ExecutionContract,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContract {
    pub revision: u32,
    pub package_manager: String,
    pub cache_args: Vec<String>,
    pub install_args: Vec<String>,
    pub verify_args: Vec<String>,
    pub common_args: Vec<String>,
    pub probe_args: Vec<Vec<String>>,
    pub seconds_per_process: u32,
    pub output_bytes_per_stream: u32,
    pub max_unpacked_bytes: u64,
    pub max_archive_entries: u32,
    pub private_paths: Vec<String>,
    pub project_paths: Vec<String>,
}

impl Default for ExecutionContract {
    fn default() -> Self {
        Self {
            revision: 1,
            package_manager: "npm".into(),
            cache_args: [
                "cache",
                "add",
                "--offline",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ]
            .map(str::to_owned)
            .to_vec(),
            install_args: [
                "ci",
                "--offline",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--no-progress",
            ]
            .map(str::to_owned)
            .to_vec(),
            verify_args: ["run", "build", "--offline", "--ignore-scripts"]
                .map(str::to_owned)
                .to_vec(),
            common_args: [
                "--cache",
                "$HOME/npm-cache",
                "--userconfig",
                "$HOME/user.npmrc",
                "--globalconfig",
                "$HOME/global.npmrc",
                "--update-notifier=false",
                "--loglevel=error",
            ]
            .map(str::to_owned)
            .to_vec(),
            probe_args: vec![vec!["--version".into()], vec!["--version".into()]],
            seconds_per_process: 300,
            output_bytes_per_stream: 256 * 1024,
            max_unpacked_bytes: 2 * 1024 * 1024 * 1024,
            max_archive_entries: 100_000,
            private_paths: [
                "runtime",
                "artifacts",
                "stage",
                "scratch",
                "scratch/home",
                "scratch/tmp",
                "scratch/home/npm-cache",
            ]
            .map(str::to_owned)
            .to_vec(),
            project_paths: [
                "package.json",
                "package-lock.json",
                "app/layout.js",
                "app/page.js",
                "node_modules",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub revision: u32,
    pub name: String,
    pub platform: String,
    pub candidates: Vec<Candidate>,
}

impl Profile {
    pub fn validate(&self) -> Result<(), Error> {
        if self.revision != 1
            || self.name != "nextjs"
            || self.platform != "darwin-arm64"
            || self.candidates.is_empty()
            || self.candidates.len() > 3
        {
            return Err(Error::Unsupported);
        }
        for candidate in &self.candidates {
            candidate.validate_lock()?;
            candidate.node.validate()?;
            if candidate.node.name != "node"
                || !is_digest(&candidate.lock_sha256)
                || semver::Version::parse(&candidate.next).is_err()
                || semver::Version::parse(&candidate.node.version).is_err()
                || semver::Version::parse(&candidate.npm_version).is_err()
                || candidate.execution != ExecutionContract::default()
                || candidate.manifest.len() > 64 * 1024
                || candidate.lock.len() > 1024 * 1024
                || format!("{:x}", Sha256::digest(candidate.lock.as_bytes()))
                    != candidate.lock_sha256
                || candidate.packages.is_empty()
                || candidate.packages.len() > 256
            {
                return Err(Error::InvalidProfile);
            }
            let mut names = BTreeSet::new();
            let mut bytes = candidate.node.bytes;
            for artifact in &candidate.packages {
                artifact.validate()?;
                if !names.insert(&artifact.name) || artifact.name == "node" {
                    return Err(Error::InvalidProfile);
                }
                bytes = bytes
                    .checked_add(artifact.bytes)
                    .ok_or(Error::InvalidProfile)?;
            }
            if bytes > 768 * 1024 * 1024
                || !candidate
                    .packages
                    .iter()
                    .any(|a| a.name == "next" && a.version == candidate.next)
            {
                return Err(Error::InvalidProfile);
            }
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, Error> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::InvalidProfile)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    /// Choose only from the bundled, reviewed tuples. A request never supplies
    /// an executable, registry URL, command, or artifact digest.
    pub fn candidate(&self, request: &str) -> Result<&Candidate, Error> {
        self.validate()?;
        let requirement = if let Ok(exact) = semver::Version::parse(request) {
            semver::VersionReq::parse(&format!("={exact}"))
        } else {
            semver::VersionReq::parse(request)
        }
        .map_err(|_| Error::Unsupported)?;
        self.candidates
            .iter()
            .find(|candidate| {
                semver::Version::parse(&candidate.next)
                    .is_ok_and(|version| requirement.matches(&version))
            })
            .ok_or(Error::Unsupported)
    }
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Persisted progress is monotonic. Unknown work must be reconciled by its
/// owner; calling set again cannot authorize repeating network/process effects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Prepared,
    Started,
    Staged,
    PublishIntent,
    Published,
    Verified,
}

/// Only a measured runtime compatibility mismatch can select another reviewed
/// tuple. Ordinary command failures must retain their real cause.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    RuntimeIncompatible,
    #[serde(alias = "next_incompatible")]
    PrimaryIncompatible,
    Transport,
    Authentication,
    Permission,
    Disk,
    Digest,
    Build,
    Cancelled,
}

impl Profile {
    pub fn fallback(
        &self,
        current: usize,
        failure: Failure,
        new_unpinned: bool,
        requested: &str,
    ) -> Result<Option<usize>, Error> {
        self.validate()?;
        let before = self.candidates.get(current).ok_or(Error::StalePlan)?;
        if failure == Failure::RuntimeIncompatible {
            return Ok(self
                .candidates
                .iter()
                .enumerate()
                .skip(current + 1)
                .find(|(_, candidate)| {
                    candidate.next == before.next
                        && candidate.lock_sha256 == before.lock_sha256
                        && candidate.packages == before.packages
                })
                .map(|(index, _)| index));
        }
        if failure != Failure::PrimaryIncompatible
            || !new_unpinned
            || semver::Version::parse(requested).is_ok()
        {
            return Ok(None);
        }
        let requirement = semver::VersionReq::parse(requested).map_err(|_| Error::Unsupported)?;
        let original = semver::Version::parse(&before.next).map_err(|_| Error::InvalidProfile)?;
        Ok(self
            .candidates
            .iter()
            .enumerate()
            .skip(current + 1)
            .find(|(_, candidate)| {
                semver::Version::parse(&candidate.next).is_ok_and(|version| {
                    version.major == original.major
                        && requirement.matches(&version)
                        && candidate.next != before.next
                })
            })
            .map(|(index, _)| index))
    }
}

impl Stage {
    pub fn advance(self, next: Self) -> Result<Self, Error> {
        let valid = matches!(
            (self, next),
            (Self::Prepared, Self::Started)
                | (Self::Started, Self::Staged)
                | (Self::Staged, Self::PublishIntent)
                | (Self::PublishIntent, Self::Published)
                | (Self::Published, Self::Verified)
        );
        if valid {
            Ok(next)
        } else {
            Err(Error::RecoveryRequired)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_cannot_repeat_or_skip_effects() {
        assert_eq!(Stage::Prepared.advance(Stage::Started), Ok(Stage::Started));
        assert_eq!(
            Stage::Started.advance(Stage::Started),
            Err(Error::RecoveryRequired)
        );
        assert_eq!(
            Stage::Prepared.advance(Stage::Published),
            Err(Error::RecoveryRequired)
        );
        assert_eq!(
            Stage::Verified.advance(Stage::Started),
            Err(Error::RecoveryRequired)
        );
    }

    #[test]
    fn artifact_rejects_unapproved_origins_and_credentials() {
        let artifact = Artifact {
            name: "node".into(),
            version: "24.22.1".into(),
            url: "https://nodejs.org/dist/fixed.tar.gz".into(),
            bytes: 10,
            sha256: "a".repeat(64),
        };
        assert_eq!(artifact.validate(), Ok(()));
        for url in [
            "http://nodejs.org/a",
            "https://nodejs.org.evil.test/a",
            "https://token@nodejs.org/a",
            "https://nodejs.org/a?token=x",
            "https://nodejs.org:8443/a",
        ] {
            assert_eq!(
                Artifact {
                    url: url.into(),
                    ..artifact.clone()
                }
                .validate(),
                Err(Error::InvalidProfile)
            );
        }
    }
}
