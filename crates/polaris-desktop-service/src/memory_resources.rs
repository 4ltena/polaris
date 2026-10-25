//! Trusted-owner strict10 preflight. No project configuration or provider requests.
use crate::TrustedMemoryIndex;
use polaris_core::{
    config::EmbeddingConfig,
    desktop_store::{BootstrapProvider, MemoryResources},
};
use polaris_desktop_protocol::snapshot::HistoryMode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{File, Metadata},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Path,
};

const MODEL: &str = "intfloat/multilingual-e5-small";
const MAX_CONFIG: u64 = 64 * 1024;
const MAX_MANIFEST: u64 = 1024 * 1024;
const MAX_RUNTIME: u64 = 256 * 1024 * 1024;
const MAX_MODEL_FILE: u64 = 512 * 1024 * 1024;
const MAX_MODEL_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_MODEL_FILES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResourceError {
    #[error(
        "structured10 is unavailable in the desktop app because selection and display are not connected. It can be selected explicitly in CLI and TUI"
    )]
    StructuredUnavailable,
    #[error(
        "Use strict10 with a Codex or OpenAI connection. A local primary connection is unsupported"
    )]
    Provider,
    #[error(
        "strict10 requires automatically installed resources, or [embedding] runtime, model_path, and revision in the OS user's ~/.polaris/config.toml"
    )]
    Configuration,
    #[error(
        "Specify existing absolute paths for strict10 resources. Links, relative paths, and execution resources within the project are not allowed"
    )]
    Path,
    #[error(
        "Cannot verify the strict10 Python executable. Specify an executable macOS binary owned by the user or root"
    )]
    Runtime,
    #[error(
        "strict10 model manifest, revision, or file SHA-256 mismatch. Check the pinned snapshot"
    )]
    Manifest,
    #[error(
        "strict10 requires a pinned snapshot of 384-dimensional multilingual-e5-small (BertModel)"
    )]
    Architecture,
    #[error("strict10 resource size or file count exceeds inspection limits")]
    Limit,
    #[error("Cannot prepare the strict10 pinned helper and dedicated storage")]
    Helper,
    #[error(
        "Cannot load strict10 Python, torch, transformers, tokenizers, or safetensors offline. Check the runtime configuration"
    )]
    Readiness,
    #[error(
        "Execution resources changed during strict10 inspection. Verify the pinned snapshot and reconnect"
    )]
    Changed,
}

impl ResourceError {
    /// Fixed startup-only exit contract; never transmits paths or upstream errors.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::StructuredUnavailable => 79,
            Self::Provider => 80,
            Self::Configuration => 81,
            Self::Path => 82,
            Self::Runtime => 83,
            Self::Manifest => 84,
            Self::Architecture => 85,
            Self::Limit => 86,
            Self::Helper => 87,
            Self::Readiness => 88,
            Self::Changed => 89,
        }
    }
}

#[derive(Deserialize)]
struct TrustedConfig {
    #[serde(default)]
    embedding: EmbeddingConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    revision: String,
    files: BTreeMap<String, String>,
}

struct Verified {
    config: EmbeddingConfig,
    runtime_hash: String,
    manifest_hash: String,
}

/// `home` is supplied only by OS account lookup, after bootstrap identity checks.
/// Legacy returns before any resource read, directory creation or process start.
pub(crate) fn prepare(
    mode: HistoryMode,
    provider: BootstrapProvider,
    home: &Path,
    store: &Path,
    source: &Path,
) -> Result<Option<TrustedMemoryIndex>, ResourceError> {
    if mode.is_structured() {
        return Err(ResourceError::StructuredUnavailable);
    }
    if mode == HistoryMode::Legacy {
        return Ok(None);
    }
    if !matches!(
        provider,
        BootstrapProvider::Codex | BootstrapProvider::Openai
    ) {
        return Err(ResourceError::Provider);
    }
    let mut config = load_config(home)?;
    // A writable Python path from either legacy managed-resources.toml or the
    // user's config is never a fallback for the package-trusted runtime image.
    if config.runtime.is_some() {
        return Err(ResourceError::Runtime);
    }
    let runtime = std::sync::Arc::new(
        polaris_core::managed_runtime::ManagedRuntimeLease::acquire(home)
            .map_err(|_| ResourceError::Runtime)?,
    );
    config.runtime = Some(runtime.python_path());
    prepare_with_config(
        mode,
        provider,
        config,
        store,
        source,
        |embedder| {
            tokio::runtime::Handle::try_current()
                .map_err(|_| ResourceError::Readiness)?
                .block_on(embedder.preflight())
                .map_err(|_| ResourceError::Readiness)
        },
        Some(runtime),
    )
}

#[cfg(test)]
fn prepare_with(
    mode: HistoryMode,
    provider: BootstrapProvider,
    home: &Path,
    store: &Path,
    source: &Path,
    probe: impl FnOnce(
        &polaris_core::conversation_memory::LocalStdioEmbedder,
    ) -> Result<serde_json::Value, ResourceError>,
) -> Result<Option<TrustedMemoryIndex>, ResourceError> {
    if mode.is_structured() {
        return Err(ResourceError::StructuredUnavailable);
    }
    if mode == HistoryMode::Legacy {
        return Ok(None);
    }
    if !matches!(
        provider,
        BootstrapProvider::Codex | BootstrapProvider::Openai
    ) {
        return Err(ResourceError::Provider);
    }
    let config = load_config(home)?;
    prepare_with_config(mode, provider, config, store, source, probe, None)
}

fn prepare_with_config(
    mode: HistoryMode,
    provider: BootstrapProvider,
    config: EmbeddingConfig,
    store: &Path,
    source: &Path,
    probe: impl FnOnce(
        &polaris_core::conversation_memory::LocalStdioEmbedder,
    ) -> Result<serde_json::Value, ResourceError>,
    runtime: Option<std::sync::Arc<polaris_core::managed_runtime::ManagedRuntimeLease>>,
) -> Result<Option<TrustedMemoryIndex>, ResourceError> {
    if mode.is_structured() {
        return Err(ResourceError::StructuredUnavailable);
    }
    if mode == HistoryMode::Legacy {
        return Ok(None);
    }
    if !matches!(
        provider,
        BootstrapProvider::Codex | BootstrapProvider::Openai
    ) {
        return Err(ResourceError::Provider);
    }
    let verified = verify(config, source)?;
    protected_open(store, true)?;
    let database = store.join("conversation-memory.sqlite3");
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let path = store.join(format!("conversation-memory.sqlite3{suffix}"));
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Ok(_) => {
                let file = protected_open(&path, false)?;
                let metadata = file.metadata().map_err(|_| ResourceError::Path)?;
                if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                    return Err(ResourceError::Path);
                }
            }
            Err(_) => return Err(ResourceError::Path),
        }
    }
    let mut embedder = polaris_core::strict_provider::local_embedder(&verified.config, store)
        .map_err(|_| ResourceError::Helper)?;
    if let Some(runtime) = runtime {
        embedder = embedder.with_pre_spawn_guard(std::sync::Arc::new(move || {
            runtime
                .validate_before_spawn()
                .map_err(std::io::Error::other)
        }));
    }
    let helper_hash = file_hash(&embedder.helper, 256 * 1024)?.0;
    let receipt = probe(&embedder)?;
    validate_receipt(&receipt, verified.config.revision.as_deref().unwrap())?;
    let current = verify(verified.config.clone(), source)?;
    if current.runtime_hash != verified.runtime_hash
        || current.manifest_hash != verified.manifest_hash
        || file_hash(&embedder.helper, 256 * 1024)?.0 != helper_hash
    {
        return Err(ResourceError::Changed);
    }
    let identity_body = serde_json::json!({
        "schema_version":1, "runtime":verified.config.runtime, "runtime_sha256":verified.runtime_hash,
        "helper_sha256":helper_hash, "model_path":verified.config.model_path,
        "manifest_sha256":verified.manifest_hash, "preflight":receipt,
    });
    let identity = MemoryResources {
        embedding_model: MODEL.into(),
        embedding_revision: verified.config.revision.unwrap(),
        embedding_dimension: 384,
        fingerprint: hex_hash(
            &serde_json::to_vec(&identity_body).map_err(|_| ResourceError::Readiness)?,
        ),
    };
    Ok(Some(TrustedMemoryIndex {
        embedder,
        database,
        identity,
    }))
}

fn validate_receipt(value: &serde_json::Value, revision: &str) -> Result<(), ResourceError> {
    let object = value.as_object().ok_or(ResourceError::Readiness)?;
    let packages = value
        .get("packages")
        .and_then(|v| v.as_object())
        .ok_or(ResourceError::Readiness)?;
    if object.len() != 4
        || value["model"] != MODEL
        || value["dimension"] != 384
        || value["revision"] != revision
        || packages.len() != 4
        || ["torch", "transformers", "tokenizers", "safetensors"]
            .iter()
            .any(|key| {
                packages
                    .get(*key)
                    .and_then(|v| v.as_str())
                    .is_none_or(|s| s.is_empty() || s.len() > 128)
            })
    {
        return Err(ResourceError::Readiness);
    }
    Ok(())
}

fn load_config(home: &Path) -> Result<EmbeddingConfig, ResourceError> {
    let directory =
        protected_open(&home.join(".polaris"), true).map_err(|_| ResourceError::Configuration)?;
    let metadata = directory
        .metadata()
        .map_err(|_| ResourceError::Configuration)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(ResourceError::Configuration);
    }
    let mut embedding = EmbeddingConfig::default();
    for name in ["managed-resources.toml", "config.toml"] {
        let path = home.join(".polaris").join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ResourceError::Configuration),
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => return Err(ResourceError::Configuration),
        };
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(ResourceError::Configuration);
        }
        let (body, opened) =
            read_bounded(&path, MAX_CONFIG).map_err(|_| ResourceError::Configuration)?;
        if opened.uid() != metadata.uid() || opened.ino() != metadata.ino() {
            return Err(ResourceError::Configuration);
        }
        let config: TrustedConfig =
            toml::from_str(std::str::from_utf8(&body).map_err(|_| ResourceError::Configuration)?)
                .map_err(|_| ResourceError::Configuration)?;
        merge_embedding(&mut embedding, config.embedding);
    }
    if embedding.model_path.is_none()
        || embedding
            .revision
            .as_ref()
            .is_none_or(|r| r.trim().is_empty() || r.len() > 256)
    {
        return Err(ResourceError::Configuration);
    }
    Ok(embedding)
}

fn merge_embedding(target: &mut EmbeddingConfig, source: EmbeddingConfig) {
    if source.runtime.is_some() {
        target.runtime = source.runtime;
    }
    if source.model_path.is_some() {
        target.model_path = source.model_path;
    }
    if source.revision.is_some() {
        target.revision = source.revision;
    }
    if source.preselect_model_path.is_some() {
        target.preselect_model_path = source.preselect_model_path;
    }
    if source.preselect_revision.is_some() {
        target.preselect_revision = source.preselect_revision;
    }
}

fn verify(config: EmbeddingConfig, source: &Path) -> Result<Verified, ResourceError> {
    let runtime = config
        .runtime
        .as_deref()
        .ok_or(ResourceError::Configuration)?;
    let model = config
        .model_path
        .as_deref()
        .ok_or(ResourceError::Configuration)?;
    if runtime.starts_with(source) || model.starts_with(source) {
        return Err(ResourceError::Path);
    }
    let (runtime_hash, metadata) =
        file_hash(runtime, MAX_RUNTIME).map_err(|_| ResourceError::Runtime)?;
    if !matches!(metadata.uid(), 0) && metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o002 != 0
    {
        return Err(ResourceError::Runtime);
    }
    let mut binary = protected_open(runtime, false).map_err(|_| ResourceError::Runtime)?;
    let mut magic = [0; 4];
    binary
        .read_exact(&mut magic)
        .map_err(|_| ResourceError::Runtime)?;
    if !matches!(
        magic,
        [0xce, 0xfa, 0xed, 0xfe]
            | [0xcf, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xce]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xca, 0xfe, 0xba, 0xbe]
            | [0xbe, 0xba, 0xfe, 0xca]
            | [0xca, 0xfe, 0xba, 0xbf]
            | [0xbf, 0xba, 0xfe, 0xca]
    ) {
        return Err(ResourceError::Runtime);
    }
    protected_open(model, true)?;
    let (manifest_bytes, _) = read_bounded(&model.join("manifest.json"), MAX_MANIFEST)
        .map_err(|_| ResourceError::Manifest)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| ResourceError::Manifest)?;
    if Some(&manifest.revision) != config.revision.as_ref()
        || manifest.files.is_empty()
        || !manifest.files.contains_key("config.json")
        || !manifest.files.keys().any(|p| p.ends_with(".safetensors"))
        || !manifest.files.keys().any(|p| p.contains("tokenizer"))
    {
        return Err(ResourceError::Manifest);
    }
    if manifest.files.len() > MAX_MODEL_FILES {
        return Err(ResourceError::Limit);
    }
    let mut found = Vec::new();
    collect_model_files(model, model, 0, &mut 0, &mut found)?;
    let expected: Vec<_> = manifest.files.keys().cloned().collect();
    found.sort();
    if found != expected {
        return Err(ResourceError::Manifest);
    }
    let mut total = 0u64;
    for (relative, expected_hash) in &manifest.files {
        if !valid_relative(relative)
            || expected_hash.len() != 64
            || !expected_hash.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ResourceError::Manifest);
        }
        let (actual, metadata) = file_hash(&model.join(relative), MAX_MODEL_FILE)
            .map_err(|_| ResourceError::Manifest)?;
        total = total
            .checked_add(metadata.len())
            .ok_or(ResourceError::Limit)?;
        if total > MAX_MODEL_TOTAL {
            return Err(ResourceError::Limit);
        }
        if actual != expected_hash.to_ascii_lowercase() {
            return Err(ResourceError::Manifest);
        }
    }
    let architecture: serde_json::Value =
        serde_json::from_slice(&read_bounded(&model.join("config.json"), MAX_MANIFEST)?.0)
            .map_err(|_| ResourceError::Architecture)?;
    if architecture["hidden_size"] != 384
        || architecture["model_type"] != "bert"
        || architecture["architectures"] != serde_json::json!(["BertModel"])
        || architecture.get("auto_map").is_some()
    {
        return Err(ResourceError::Architecture);
    }
    Ok(Verified {
        config,
        runtime_hash,
        manifest_hash: hex_hash(&manifest_bytes),
    })
}

fn collect_model_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    entries: &mut usize,
    out: &mut Vec<String>,
) -> Result<(), ResourceError> {
    if depth > 8 {
        return Err(ResourceError::Limit);
    }
    protected_open(directory, true)?;
    for entry in std::fs::read_dir(directory).map_err(|_| ResourceError::Manifest)? {
        let entry = entry.map_err(|_| ResourceError::Manifest)?;
        *entries += 1;
        if *entries > MAX_MODEL_FILES * 4 {
            return Err(ResourceError::Limit);
        }
        let kind = entry.file_type().map_err(|_| ResourceError::Manifest)?;
        let path = entry.path();
        if kind.is_dir() {
            collect_model_files(root, &path, depth + 1, entries, out)?;
        } else if kind.is_file() {
            if path == root.join("manifest.json") {
                continue;
            }
            out.push(
                path.strip_prefix(root)
                    .map_err(|_| ResourceError::Path)?
                    .to_str()
                    .ok_or(ResourceError::Path)?
                    .into(),
            );
            if out.len() > MAX_MODEL_FILES {
                return Err(ResourceError::Limit);
            }
        } else {
            return Err(ResourceError::Path);
        }
    }
    Ok(())
}

fn valid_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Walk every component without following a link; the final FD owns the read.
fn protected_open(path: &Path, directory: bool) -> Result<File, ResourceError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > 4096
        || bytes.first() != Some(&b'/')
        || bytes.len() == 1
        || bytes[1..]
            .split(|b| *b == b'/')
            .any(|c| c.is_empty() || c == b"." || c == b".." || c.contains(&0))
    {
        return Err(ResourceError::Path);
    }
    let parts: Vec<_> = bytes[1..].split(|b| *b == b'/').collect();
    let mut parent = File::open("/").map_err(|_| ResourceError::Path)?;
    for (index, part) in parts.iter().enumerate() {
        let name = CString::new(*part).map_err(|_| ResourceError::Path)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory || index + 1 < parts.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: owned directory FD, NUL-free single component and fixed flags.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(ResourceError::Path);
        }
        // SAFETY: successful openat transferred a fresh descriptor.
        parent = unsafe { File::from_raw_fd(fd) };
    }
    let metadata = parent.metadata().map_err(|_| ResourceError::Path)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(ResourceError::Path);
    }
    Ok(parent)
}

fn read_bounded(path: &Path, limit: u64) -> Result<(Vec<u8>, Metadata), ResourceError> {
    let mut file = protected_open(path, false)?;
    let metadata = file.metadata().map_err(|_| ResourceError::Path)?;
    if metadata.len() > limit {
        return Err(ResourceError::Limit);
    }
    let mut body = Vec::new();
    (&mut file)
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|_| ResourceError::Path)?;
    if body.len() as u64 != metadata.len()
        || !same_file(
            &metadata,
            &file.metadata().map_err(|_| ResourceError::Path)?,
        )
    {
        return Err(ResourceError::Changed);
    }
    Ok((body, metadata))
}

fn file_hash(path: &Path, limit: u64) -> Result<(String, Metadata), ResourceError> {
    let mut file = protected_open(path, false)?;
    let before = file.metadata().map_err(|_| ResourceError::Path)?;
    if before.len() > limit {
        return Err(ResourceError::Limit);
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0u64;
    loop {
        let count = file.read(&mut buffer).map_err(|_| ResourceError::Path)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > limit {
            return Err(ResourceError::Limit);
        }
        hash.update(&buffer[..count]);
    }
    if total != before.len()
        || !same_file(&before, &file.metadata().map_err(|_| ResourceError::Path)?)
    {
        return Err(ResourceError::Changed);
    }
    Ok((format!("{:x}", hash.finalize()), before))
}
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}
fn hex_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
