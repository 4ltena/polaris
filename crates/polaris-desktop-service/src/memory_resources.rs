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
        "strict10はCodexまたはOpenAI接続で使用してください。ローカル主接続には対応していません"
    )]
    Provider,
    #[error(
        "strict10にはOSユーザーの ~/.polaris/config.toml に [embedding] runtime・model_path・revision の設定が必要です"
    )]
    Configuration,
    #[error(
        "strict10資源には実在する絶対パスを指定してください。リンク・相対パス・プロジェクト内の実行資源は使用できません"
    )]
    Path,
    #[error(
        "strict10のPython実行物を確認できません。本人またはroot所有の実行可能なmacOSバイナリを指定してください"
    )]
    Runtime,
    #[error(
        "strict10モデルのmanifest・revision・全ファイルのSHA-256が一致しません。固定snapshotを確認してください"
    )]
    Manifest,
    #[error("strict10には384次元のmultilingual-e5-small（BertModel）の固定snapshotが必要です")]
    Architecture,
    #[error("strict10の資源サイズ・ファイル数が検査上限を超えています")]
    Limit,
    #[error("strict10の固定helperと専用保存先を準備できません")]
    Helper,
    #[error(
        "strict10のPython・torch・transformers・tokenizers・safetensorsをofflineで読み込めません。runtimeの構成を確認してください"
    )]
    Readiness,
    #[error(
        "strict10の検査中に実行資源が変更されました。固定snapshotを確認して接続し直してください"
    )]
    Changed,
}

impl ResourceError {
    /// Fixed startup-only exit contract; never transmits paths or upstream errors.
    pub fn exit_code(self) -> i32 {
        match self {
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
    prepare_with(mode, provider, home, store, source, |embedder| {
        tokio::runtime::Handle::try_current()
            .map_err(|_| ResourceError::Readiness)?
            .block_on(embedder.preflight())
            .map_err(|_| ResourceError::Readiness)
    })
}

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
    let embedder = polaris_core::strict_provider::local_embedder(&verified.config, store)
        .map_err(|_| ResourceError::Helper)?;
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
    let (body, metadata) = read_bounded(&home.join(".polaris/config.toml"), MAX_CONFIG)
        .map_err(|_| ResourceError::Configuration)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(ResourceError::Configuration);
    }
    let config: TrustedConfig =
        toml::from_str(std::str::from_utf8(&body).map_err(|_| ResourceError::Configuration)?)
            .map_err(|_| ResourceError::Configuration)?;
    let embedding = config.embedding;
    if embedding.runtime.is_none()
        || embedding.model_path.is_none()
        || embedding
            .revision
            .as_ref()
            .is_none_or(|r| r.trim().is_empty() || r.len() > 256)
    {
        return Err(ResourceError::Configuration);
    }
    Ok(embedding)
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
