//! Bounded native-owner bootstrap reader. Validated metadata is not a runtime
//! grant: this module neither creates policy nor opens credentials/providers.
use super::{Published, StoreError, StoreResult, disk::Directory};
use polaris_desktop_protocol::ids::{DecimalU64, ProjectId, SessionId};
use polaris_provider::local::Endpoint;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

pub const MAX_OWNER_BOOTSTRAP_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapIdentity {
    pub device: DecimalU64,
    pub inode: DecimalU64,
}
impl BootstrapIdentity {
    fn matches(self, (device, inode): (u64, u64)) -> bool {
        self.device.get() == device && self.inode.get() == inode
    }
}

/// Tiers describe only the confirmed permission ceiling. Approval policy and
/// automatic folder grants remain execution-owner decisions; this reader neither
/// requires an extra approval nor converts tiers into sandbox/tool capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapTier {
    ReadOnly,
    ReadCreate,
    ReadCreateBuild,
    ReadCreateBuildExternal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootstrapProvider {
    Codex,
    Openai,
    Ollama,
    Lmstudio,
}
impl BootstrapProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Openai => "openai",
            Self::Ollama => "ollama",
            Self::Lmstudio => "lmstudio",
        }
    }
}

/// Untrusted wire document. Only read_owner_bootstrap returns validated values.
/// Paths identify existing objects; no path is an instruction to create/open a
/// credential, helper, provider or source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerBootstrapDocument {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub store_identity: BootstrapIdentity,
    pub source_path: String,
    pub source_identity: BootstrapIdentity,
    pub tier: BootstrapTier,
    pub policy_revision: DecimalU64,
    pub configuration_revision: DecimalU64,
    pub provider: BootstrapProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_endpoint: Option<String>,
}

/// Expected identity and digest come from the native publication, not its body.
pub struct ExpectedBootstrapFile {
    pub identity: BootstrapIdentity,
    pub sha256: [u8; 32],
}
/// The trusted owner supplies the already selected store root and pinned identity.
/// The bootstrap must be a direct child of that root, not an arbitrary file.
pub struct ExpectedBootstrapStore<'a> {
    pub path: &'a Path,
    pub identity: BootstrapIdentity,
}

#[derive(Debug)]
pub struct ValidatedOwnerBootstrap {
    document: OwnerBootstrapDocument,
    model: String,
    effort: String,
}
impl ValidatedOwnerBootstrap {
    pub fn document(&self) -> &OwnerBootstrapDocument {
        &self.document
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    /// Compatibility value for the existing durable configuration, not evidence
    /// that a local model supports or applies a reasoning effort.
    pub fn stored_effort(&self) -> &str {
        &self.effort
    }
    /// Only cloud providers have an effective effort in this bootstrap contract.
    pub fn effective_effort(&self) -> Option<&str> {
        matches!(
            self.document.provider,
            BootstrapProvider::Codex | BootstrapProvider::Openai
        )
        .then_some(self.effort.as_str())
    }
}

fn refused() -> StoreError {
    StoreError::Corrupt("owner bootstrap")
}
fn bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
/// Validate saved owner configuration metadata only; no runtime or capability
/// claim. Local `medium` is the legacy storage value, never effective effort.
pub fn validate_owner_configuration(
    provider: &str,
    model: &str,
    stored_effort: &str,
) -> StoreResult<()> {
    let cloud = match provider {
        "codex" | "openai" => true,
        "ollama" | "lmstudio" => false,
        _ => return Err(refused()),
    };
    if !bounded_text(model, 512)
        || (cloud
            && !matches!(
                stored_effort,
                "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
            ))
        || (!cloud && stored_effort != "medium")
    {
        return Err(refused());
    }
    Ok(())
}
fn absolute(value: &str) -> bool {
    bounded_text(value, 4096)
        && value.starts_with('/')
        && value != "/"
        && !value
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
        && !Path::new(value)
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
}
fn validate(
    document: OwnerBootstrapDocument,
    store: &ExpectedBootstrapStore<'_>,
    saved: &Published,
) -> StoreResult<ValidatedOwnerBootstrap> {
    if document.schema_version != 1
        || saved.marker.deleted
        || document.project_id != saved.marker.project_id
        || document.session_id != saved.marker.session_id
        || document.store_identity != store.identity
        || document.policy_revision != saved.state.policy_revision
        || document.configuration_revision != saved.state.configuration.configuration_revision
        || !absolute(&document.source_path)
        || !bounded_text(document.project_id.as_str(), 128)
        || !bounded_text(document.session_id.as_str(), 128)
    {
        return Err(refused());
    }
    let cloud = matches!(
        document.provider,
        BootstrapProvider::Codex | BootstrapProvider::Openai
    );
    let model = match &document.model {
        Some(model) if bounded_text(model, 512) => model.clone(),
        None if cloud => "gpt-6-astra".into(),
        _ => return Err(refused()),
    };
    if !cloud && document.effort.is_some() {
        return Err(refused());
    }
    let effort = document.effort.clone().unwrap_or_else(|| "medium".into());
    validate_owner_configuration(document.provider.as_str(), &model, &effort)?;
    match (cloud, &document.local_endpoint) {
        (true, None) => (),
        (false, Some(endpoint)) => {
            let parsed = Endpoint::parse(endpoint).map_err(|_| refused())?;
            if parsed.as_str() != endpoint {
                return Err(refused());
            }
        }
        _ => return Err(refused()),
    }
    let configuration = &saved.state.configuration;
    if configuration.provider != document.provider.as_str()
        || configuration.model != model
        || configuration.effort != effort
    {
        return Err(refused());
    }
    Ok(ValidatedOwnerBootstrap {
        document,
        model,
        effort,
    })
}

/// Read only the exact bounded publication. No grants are minted or updated.
/// Source identity and tier remain native metadata: the runtime owner must still
/// validate current source identity and enforce the confirmed capabilities.
#[cfg(unix)]
pub fn read_owner_bootstrap(
    path: &Path,
    expected: &ExpectedBootstrapFile,
    store: &ExpectedBootstrapStore<'_>,
    saved: &Published,
) -> StoreResult<ValidatedOwnerBootstrap> {
    read_checked(path, expected, store, saved, || {})
}
#[cfg(unix)]
fn read_checked(
    path: &Path,
    expected: &ExpectedBootstrapFile,
    store: &ExpectedBootstrapStore<'_>,
    saved: &Published,
    after_read: impl FnOnce(),
) -> StoreResult<ValidatedOwnerBootstrap> {
    use sha2::{Digest, Sha256};
    use std::{io::Read, os::unix::fs::MetadataExt};
    if path.parent() != Some(store.path)
        || !path.to_str().is_some_and(absolute)
        || !store.path.to_str().is_some_and(absolute)
    {
        return Err(refused());
    }
    let parent = Directory::open_owned(store.path)?;
    if !store.identity.matches(parent.identity()?) {
        return Err(refused());
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(refused)?;
    let mut file = parent.open(name, false, false)?;
    let before = file.metadata()?;
    if !expected.identity.matches((before.dev(), before.ino()))
        || before.len() > MAX_OWNER_BOOTSTRAP_BYTES as u64
    {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_OWNER_BOOTSTRAP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    after_read();
    let after = file.metadata()?;
    let named = parent.open(name, false, false)?.metadata()?;
    parent.verify()?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    if bytes.len() > MAX_OWNER_BOOTSTRAP_BYTES
        || !unchanged(&before, &after)
        || !unchanged(&after, &named)
        || digest != expected.sha256
    {
        return Err(refused());
    }
    let document: OwnerBootstrapDocument = serde_json::from_slice(&bytes)?;
    validate(document, store, saved)
}
#[cfg(unix)]
fn unchanged(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
#[cfg(not(unix))]
pub fn read_owner_bootstrap(
    _: &Path,
    _: &ExpectedBootstrapFile,
    _: &ExpectedBootstrapStore<'_>,
    _: &Published,
) -> StoreResult<ValidatedOwnerBootstrap> {
    Err(StoreError::UnsupportedPlatform)
}

#[cfg(all(test, unix))]
mod tests;
