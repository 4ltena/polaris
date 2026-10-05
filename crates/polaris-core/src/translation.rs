//! Offline-only adapter for the pinned CAT-Translate 3.3B helper.
//!
//! This is deliberately below answer finalization: it translates already
//! protected text and returns an observation.  It cannot publish a message,
//! mutate history, invoke a provider, or fall back to a network runtime.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

pub const CAT_TRANSLATE_MODEL: &str = "cyberagent/CAT-Translate-3.3b";
pub const CAT_TRANSLATE_REVISION: &str = "47e382331d005acd54a42cdf088a76aa88788e0c";
pub const MAX_TRANSLATION_INPUT_BYTES: usize = 32 * 1024;
pub const MAX_TRANSLATION_OUTPUT_BYTES: usize = 16 * 1024;
pub const MAX_TRANSLATION_INPUT_TOKENS: u32 = 2_048;
pub const MAX_TRANSLATION_OUTPUT_TOKENS: u32 = 1_024;
const MAX_HELPER_OUTPUT_BYTES: u64 = 256 * 1024;
const MAX_HELPER_ERROR_BYTES: u64 = 16 * 1024;
const MAX_DEPENDENCY_MANIFEST_BYTES: usize = 512 * 1024;
const MAX_MODEL_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_INSTALL_RECEIPT_BYTES: u64 = 16 * 1024;
const MAX_MODEL_FILES: usize = 64;
const MAX_MODEL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const BUNDLED_RUNTIME_MANIFEST: &[u8] = include_bytes!(
    "../../../apps/macos/Sources/PolarisDesktop/Resources/runtime-dependencies.json"
);

#[derive(Deserialize)]
struct RuntimeDependencyManifest {
    schema_version: u32,
    dependencies: Vec<RuntimeDependencyEntry>,
}

#[derive(Deserialize)]
struct RuntimeDependencyEntry {
    id: String,
    version: String,
    platform: String,
    architecture: String,
    #[serde(default)]
    destination: Option<String>,
    #[serde(default)]
    files: Option<Vec<RuntimeDependencyFile>>,
}

#[derive(Deserialize)]
struct RuntimeDependencyFile {
    path: String,
    sha256: String,
    size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledModelManifest {
    revision: String,
    files: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedDependencyReceipt {
    schema_version: u32,
    id: String,
    version: String,
    manifest_sha256: String,
    #[serde(default)]
    runtime_tree_sha256: Option<String>,
    installed_at: String,
}

#[derive(Clone)]
struct TrustedFileMarker {
    path: PathBuf,
    sha256: String,
    size: u64,
}

impl TrustedFileMarker {
    fn capture(path: PathBuf, sha256: String, size: u64) -> io::Result<Self> {
        let marker = Self { path, sha256, size };
        marker.validate()?;
        Ok(marker)
    }

    fn validate(&self) -> io::Result<()> {
        let before = std::fs::symlink_metadata(&self.path)?;
        if before.file_type().is_symlink()
            || !before.file_type().is_file()
            || before.len() != self.size
        {
            return Err(io::Error::other("managed translation artifact changed"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(&self.path)?;
        let opened = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
                return Err(io::Error::other("managed translation artifact changed"));
            }
        }
        let actual = sha256_reader(&mut file)?;
        let after = std::fs::symlink_metadata(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (after.dev(), after.ino(), after.len()) != (before.dev(), before.ino(), before.len())
            {
                return Err(io::Error::other("managed translation artifact changed"));
            }
        }
        if actual != self.sha256 {
            return Err(io::Error::other("managed translation artifact changed"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ManagedTranslationIdentity {
    files: Vec<TrustedFileMarker>,
    directories: Vec<PathBuf>,
    runtime: crate::managed_runtime::ManagedRuntimeLease,
}

impl ManagedTranslationIdentity {
    fn validate(&self) -> io::Result<()> {
        for path in &self.directories {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(io::Error::other("managed translation directory changed"));
            }
        }
        self.files
            .iter()
            .try_for_each(TrustedFileMarker::validate)?;
        self.runtime
            .validate_before_spawn()
            .map_err(io::Error::other)
    }
}

/// A direction is part of the profile identity.  A backend may not silently
/// reverse it simply because its underlying model supports both languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationDirection {
    EnToJa,
    JaToEn,
}

impl TranslationDirection {
    pub const fn source_language(self) -> &'static str {
        match self {
            Self::EnToJa => "en",
            Self::JaToEn => "ja",
        }
    }

    pub const fn target_language(self) -> &'static str {
        match self {
            Self::EnToJa => "ja",
            Self::JaToEn => "en",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranslationRequestV1 {
    pub direction: TranslationDirection,
    /// The finalizer must pass protected-span placeholders, never unprotected
    /// source text that it cannot subsequently verify.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranslationReceiptV1 {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub finish_reason: TranslationFinishReason,
    pub truncated: bool,
    pub model: String,
    pub revision: String,
    pub device: TranslationDevice,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationFinishReason {
    Stop,
    Length,
}

/// The helper reports the device selected by its deterministic local probe.
/// It never accepts a caller-selected fallback through the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationDevice {
    Cpu,
    Mps,
}

pub type TranslationFuture<'a> =
    Pin<Box<dyn Future<Output = io::Result<TranslationReceiptV1>> + Send + 'a>>;

/// Backend-neutral contract used by the shadow finalizer.  The result remains
/// an observation until the finalizer passes integrity and delivered judging.
pub trait TranslationBackendV1: Send + Sync {
    fn translate<'a>(&'a self, request: TranslationRequestV1) -> TranslationFuture<'a>;
}

/// Fixed-argv configuration for the installer-owned CAT snapshot.
#[derive(Debug, Clone)]
pub struct LocalCatTranslate {
    pub python: PathBuf,
    pub helper: PathBuf,
    pub model_path: PathBuf,
    pub run_temp: PathBuf,
    pub cpu_threads: usize,
    pub timeout: Duration,
}

/// Installer-owned CAT snapshot plus the read-only package runtime that is
/// allowed to load it. Both the desktop service and TUI use this owner so an
/// installed model cannot be accepted through a weaker host-specific path.
pub struct ManagedCatTranslate {
    inner: LocalCatTranslate,
    identity: ManagedTranslationIdentity,
}

impl ManagedCatTranslate {
    pub fn open(home: &Path) -> io::Result<Self> {
        Self::open_with_installation(home, &mut None)
    }

    /// Preserve model-installation evidence separately from runtime readiness.
    /// Failure leaves it unknown; no second hash pass or fallback is used.
    pub fn open_with_installation(home: &Path, installed: &mut Option<bool>) -> io::Result<Self> {
        *installed = None;
        let (files, mut directories) = verify_model_installation(home, BUNDLED_RUNTIME_MANIFEST)?;
        *installed = Some(true);
        let runtime =
            crate::managed_runtime::ManagedRuntimeLease::acquire(home).map_err(io::Error::other)?;
        let polaris = home.join(".polaris");
        let runtime_root = polaris.join("runtime");
        let private = runtime_root.join("answer-finalizer");
        let run_root = polaris.join("run");
        let run_temp = run_root.join("answer-finalizer");
        for directory in [&runtime_root, &private, &run_root, &run_temp] {
            ensure_private_runtime_directory(directory)?;
            directories.push(directory.clone());
        }
        let helper = LocalCatTranslate::install_bundled_helper(&private)?;
        directories.push(
            helper
                .parent()
                .ok_or_else(|| io::Error::other("translation helper parent is unavailable"))?
                .to_owned(),
        );
        let helper_size = std::fs::symlink_metadata(&helper)?.len();
        let mut files = files;
        files.push(TrustedFileMarker::capture(
            helper.clone(),
            sha256_file(&helper)?,
            helper_size,
        )?);
        let identity = ManagedTranslationIdentity {
            files,
            directories,
            runtime,
        };
        identity.validate()?;
        let inner = LocalCatTranslate::new(
            identity.runtime.python_path(),
            helper,
            LocalCatTranslate::default_model_path(home),
            run_temp,
            4,
            Duration::from_secs(90),
        )?;
        Ok(Self { inner, identity })
    }

    /// Prove that the pinned packages and model can be opened without network
    /// access before this backend is attached to the finalizer.
    pub async fn preflight(&self) -> io::Result<()> {
        self.identity.validate()?;
        self.inner.preflight().await
    }
}

impl TranslationBackendV1 for ManagedCatTranslate {
    fn translate<'a>(&'a self, request: TranslationRequestV1) -> TranslationFuture<'a> {
        Box::pin(async move {
            self.identity.validate()?;
            self.inner.translate(request).await
        })
    }
}

fn ensure_private_runtime_directory(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(io::Error::other(
                    "managed translation runtime directory is invalid",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| io::Error::other("translation directory has no parent"))?;
            let parent_metadata = std::fs::symlink_metadata(parent)?;
            if parent_metadata.file_type().is_symlink() || !parent_metadata.file_type().is_dir() {
                return Err(io::Error::other(
                    "managed translation runtime parent is invalid",
                ));
            }
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return ensure_private_runtime_directory(path);
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::other(
                "managed translation runtime directory is not private",
            ));
        }
    }
    Ok(())
}

fn verify_model_installation(
    home: &Path,
    manifest_bytes: &[u8],
) -> io::Result<(Vec<TrustedFileMarker>, Vec<PathBuf>)> {
    use std::path::Component;

    if !home.is_absolute() || manifest_bytes.len() > MAX_DEPENDENCY_MANIFEST_BYTES {
        return Err(io::Error::other(
            "managed translation manifest location is invalid",
        ));
    }
    let manifest_hash = sha256_bytes(manifest_bytes);
    let manifest: RuntimeDependencyManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|_| io::Error::other("managed translation manifest is invalid"))?;
    if manifest.schema_version != 1 {
        return Err(io::Error::other(
            "managed translation manifest schema is invalid",
        ));
    }
    let mut matches = manifest.dependencies.iter().filter(|entry| {
        entry.id == "cat_translate_3_3b"
            && entry.version == CAT_TRANSLATE_REVISION
            && entry.platform == "macos"
            && entry.architecture == "universal"
            && entry.destination.as_deref() == Some("models/cat-translate-3.3b")
    });
    let cat = matches
        .next()
        .ok_or_else(|| io::Error::other("managed CAT dependency is missing"))?;
    if matches.next().is_some() {
        return Err(io::Error::other("managed CAT dependency is duplicated"));
    }
    let model_files = cat
        .files
        .as_ref()
        .filter(|files| !files.is_empty() && files.len() <= MAX_MODEL_FILES)
        .ok_or_else(|| io::Error::other("managed CAT file manifest is invalid"))?;
    let mut expected_files = BTreeMap::new();
    let mut total_bytes = 0_u64;
    for file in model_files {
        let relative = Path::new(&file.path);
        if file.path.len() > 4096
            || relative.as_os_str().is_empty()
            || relative.components().count() > 64
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            || !valid_sha256(&file.sha256)
            || expected_files
                .insert(file.path.clone(), file.sha256.clone())
                .is_some()
        {
            return Err(io::Error::other("managed CAT file entry is invalid"));
        }
        total_bytes = total_bytes
            .checked_add(file.size)
            .filter(|total| *total <= MAX_MODEL_BYTES)
            .ok_or_else(|| io::Error::other("managed CAT files exceed the limit"))?;
    }

    let polaris = home.join(".polaris");
    let model_base = polaris.join("models/cat-translate-3.3b");
    let model_root = model_base.join(CAT_TRANSLATE_REVISION);
    let directories = vec![
        polaris.clone(),
        polaris.join("models"),
        model_base.clone(),
        model_root.clone(),
    ];
    for directory in &directories {
        let metadata = std::fs::symlink_metadata(directory)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(io::Error::other("managed translation directory is invalid"));
        }
    }

    let receipt_path = model_base.join(format!(".{CAT_TRANSLATE_REVISION}.receipt.json"));
    let receipt_bytes = read_bounded_regular(&receipt_path, MAX_INSTALL_RECEIPT_BYTES)?;
    let receipt: ManagedDependencyReceipt = serde_json::from_slice(&receipt_bytes)
        .map_err(|_| io::Error::other("managed CAT receipt is invalid"))?;
    if receipt.schema_version != 1
        || receipt.id != "cat_translate_3_3b"
        || receipt.version != CAT_TRANSLATE_REVISION
        || receipt.manifest_sha256 != manifest_hash
        || receipt.runtime_tree_sha256.is_some()
        || receipt.installed_at.is_empty()
        || receipt.installed_at.len() > 128
    {
        return Err(io::Error::other("managed CAT receipt does not match"));
    }
    let mut markers = vec![TrustedFileMarker::capture(
        receipt_path,
        sha256_bytes(&receipt_bytes),
        receipt_bytes.len() as u64,
    )?];

    let model_manifest_path = model_root.join("manifest.json");
    let model_manifest_bytes =
        read_bounded_regular(&model_manifest_path, MAX_MODEL_MANIFEST_BYTES)?;
    let installed: InstalledModelManifest = serde_json::from_slice(&model_manifest_bytes)
        .map_err(|_| io::Error::other("installed CAT manifest is invalid"))?;
    if installed.revision != CAT_TRANSLATE_REVISION || installed.files != expected_files {
        return Err(io::Error::other("installed CAT manifest does not match"));
    }
    markers.push(TrustedFileMarker::capture(
        model_manifest_path,
        sha256_bytes(&model_manifest_bytes),
        model_manifest_bytes.len() as u64,
    )?);
    for file in model_files {
        markers.push(TrustedFileMarker::capture(
            model_root.join(&file.path),
            file.sha256.clone(),
            file.size,
        )?);
    }
    Ok((markers, directories))
}

fn read_bounded_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > limit
    {
        return Err(io::Error::other(
            "managed translation metadata file is invalid",
        ));
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(
            "managed translation metadata exceeds the limit",
        ));
    }
    Ok(bytes)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    sha256_reader(&mut file)
}

fn sha256_reader(reader: &mut impl Read) -> io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl LocalCatTranslate {
    pub fn new(
        python: PathBuf,
        helper: PathBuf,
        model_path: PathBuf,
        run_temp: PathBuf,
        cpu_threads: usize,
        timeout: Duration,
    ) -> io::Result<Self> {
        if !python.is_absolute()
            || !helper.is_absolute()
            || !model_path.is_absolute()
            || !run_temp.is_absolute()
            || cpu_threads == 0
            || timeout.is_zero()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local CAT-Translate configuration is invalid",
            ));
        }
        Ok(Self {
            python,
            helper,
            model_path,
            run_temp,
            cpu_threads,
            timeout,
        })
    }

    /// The installer and runtime intentionally share this exact destination.
    /// `home` is supplied by the owning settings layer; this adapter never
    /// guesses a user directory or discovers models from the environment.
    pub fn default_model_path(home: &Path) -> PathBuf {
        home.join(".polaris")
            .join("models")
            .join("cat-translate-3.3b")
            .join(CAT_TRANSLATE_REVISION)
    }

    /// Materialize the reviewed helper under a caller-owned private directory.
    /// Existing bytes must be identical; a replacement or symlink fails closed.
    pub fn install_bundled_helper(private_root: &Path) -> io::Result<PathBuf> {
        if !private_root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "translation helper root must be absolute",
            ));
        }
        let directory = private_root.join("translation-helper");
        std::fs::create_dir_all(&directory)?;
        if std::fs::symlink_metadata(&directory)?
            .file_type()
            .is_symlink()
        {
            return Err(io::Error::other(
                "translation helper root cannot be a symlink",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let source = include_bytes!("../../../scripts/local_translation.py");
        let helper = directory.join(format!(
            "local_translation-{}.py",
            crate::conversation_state::content_hash(source)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&helper)
        {
            Ok(mut file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                file.write_all(source)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if std::fs::symlink_metadata(&helper)?.file_type().is_symlink()
                    || std::fs::read(&helper)? != source
                {
                    return Err(io::Error::other(
                        "existing translation helper differs from bundled source",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
        Ok(helper)
    }

    /// Bounded package/model readiness.  It only starts the explicit local
    /// executable with offline environment flags and does not translate text.
    pub async fn preflight(&self) -> io::Result<()> {
        let response = self.helper_call(None).await?;
        Self::validate_preflight(response)
    }

    fn validate_preflight(response: serde_json::Value) -> io::Result<()> {
        let expected = BTreeSet::from(["model", "revision", "device", "packages"]);
        let actual: BTreeSet<&str> = response
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let packages = response
            .get("packages")
            .and_then(serde_json::Value::as_object);
        if actual != expected
            || response.get("model").and_then(serde_json::Value::as_str)
                != Some(CAT_TRANSLATE_MODEL)
            || response.get("revision").and_then(serde_json::Value::as_str)
                != Some(CAT_TRANSLATE_REVISION)
            || !matches!(
                response.get("device").and_then(serde_json::Value::as_str),
                Some("cpu" | "mps")
            )
            || packages.is_none_or(|packages| {
                packages.len() != 5
                    || [
                        "torch",
                        "transformers",
                        "tokenizers",
                        "safetensors",
                        "sentencepiece",
                    ]
                    .iter()
                    .any(|name| {
                        packages
                            .get(*name)
                            .and_then(serde_json::Value::as_str)
                            .is_none_or(|value| value.is_empty() || value.len() > 128)
                    })
            })
        {
            return Err(io::Error::other(
                "translation preflight response does not match the pinned contract",
            ));
        }
        Ok(())
    }

    async fn translate_inner(
        &self,
        request: TranslationRequestV1,
    ) -> io::Result<TranslationReceiptV1> {
        if request.text.is_empty() || request.text.len() > MAX_TRANSLATION_INPUT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "translation input is outside the fixed limit",
            ));
        }
        let response = self
            .helper_call(Some(serde_json::to_string(&request)?))
            .await?;
        Self::parse_receipt(response)
    }

    fn parse_receipt(response: serde_json::Value) -> io::Result<TranslationReceiptV1> {
        let expected = BTreeSet::from([
            "finish_reason",
            "input_tokens",
            "latency_ms",
            "model",
            "output_tokens",
            "revision",
            "device",
            "text",
            "truncated",
        ]);
        let actual: BTreeSet<&str> = response
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if actual != expected {
            return Err(io::Error::other(
                "translation helper response shape is invalid",
            ));
        }
        let receipt: TranslationReceiptV1 = serde_json::from_value(response)
            .map_err(|_| io::Error::other("translation helper response is malformed"))?;
        if receipt.model != CAT_TRANSLATE_MODEL
            || receipt.revision != CAT_TRANSLATE_REVISION
            || receipt.text.is_empty()
            || receipt.text.len() > MAX_TRANSLATION_OUTPUT_BYTES
            || receipt.input_tokens == 0
            || receipt.input_tokens > MAX_TRANSLATION_INPUT_TOKENS
            || receipt.output_tokens == 0
            || receipt.output_tokens > MAX_TRANSLATION_OUTPUT_TOKENS
            || receipt.truncated != (receipt.finish_reason == TranslationFinishReason::Length)
        {
            return Err(io::Error::other(
                "translation helper response violates the pinned contract",
            ));
        }
        Ok(receipt)
    }

    async fn helper_call(&self, request: Option<String>) -> io::Result<serde_json::Value> {
        use std::process::Stdio;
        use tokio::{io::AsyncWriteExt, process::Command};

        let call_temp = tempfile::Builder::new()
            .prefix("translation-call-")
            .tempdir_in(&self.run_temp)?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut command = Command::new(&self.python);
        command
            .arg("-I")
            .arg("-B")
            .arg(&self.helper)
            .arg("--model-path")
            .arg(&self.model_path)
            .arg("--revision")
            .arg(CAT_TRANSLATE_REVISION)
            .kill_on_drop(true)
            .env_clear()
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", call_temp.path())
            .env("TRANSFORMERS_OFFLINE", "1")
            .env("HF_HUB_OFFLINE", "1")
            .env("PYTHONNOUSERSITE", "1")
            .env("TOKENIZERS_PARALLELISM", "false")
            .env("OMP_NUM_THREADS", self.cpu_threads.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if request.is_none() {
            command.arg("--preflight");
        }
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("translation stdin is unavailable"))?;
        let write = async {
            if let Some(request) = request {
                tokio::io::AsyncWriteExt::write_all(&mut stdin, format!("{request}\n").as_bytes())
                    .await?;
            }
            stdin.shutdown().await
        };
        match tokio::time::timeout_at(deadline, write).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                child.kill().await?;
                return Err(error);
            }
            Err(_) => {
                child.kill().await?;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "translation helper stdin timed out and was reaped",
                ));
            }
        }
        drop(stdin);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("translation stdout is unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("translation stderr is unavailable"))?;
        let out_task = tokio::spawn(read_bounded(stdout, MAX_HELPER_OUTPUT_BYTES));
        let err_task = tokio::spawn(read_bounded(stderr, MAX_HELPER_ERROR_BYTES));
        let status = tokio::select! {
            result = child.wait() => result?,
            _ = tokio::time::sleep_until(deadline) => {
                child.kill().await?;
                let _ = out_task.await;
                let _ = err_task.await;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "translation helper timed out and was reaped"));
            }
        };
        let output = out_task
            .await
            .map_err(|_| io::Error::other("translation stdout task failed"))??;
        let stderr = err_task
            .await
            .map_err(|_| io::Error::other("translation stderr task failed"))??;
        if !status.success() {
            return Err(io::Error::other(format!(
                "translation helper exited unsuccessfully: {}",
                String::from_utf8_lossy(&stderr)
            )));
        }
        let output = String::from_utf8(output)
            .map_err(|_| io::Error::other("translation helper stdout is not UTF-8"))?;
        serde_json::from_str(output.trim())
            .map_err(|_| io::Error::other("translation helper returned invalid JSON"))
    }
}

impl TranslationBackendV1 for LocalCatTranslate {
    fn translate<'a>(&'a self, request: TranslationRequestV1) -> TranslationFuture<'a> {
        Box::pin(async move { self.translate_inner(request).await })
    }
}

async fn read_bounded<R>(reader: R, limit: u64) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(
            "translation helper stream exceeds the fixed limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_model_fixture(root: &Path) -> (PathBuf, Vec<u8>, PathBuf) {
        let home = root.join("home");
        let model_base = home.join(".polaris/models/cat-translate-3.3b");
        let model_root = model_base.join(CAT_TRANSLATE_REVISION);
        std::fs::create_dir_all(&model_root).unwrap();
        let model_file = model_root.join("weights.bin");
        std::fs::write(&model_file, b"fixed model bytes").unwrap();
        let model_hash = sha256_file(&model_file).unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "dependencies": [{
                "id": "cat_translate_3_3b",
                "version": CAT_TRANSLATE_REVISION,
                "platform": "macos",
                "architecture": "universal",
                "destination": "models/cat-translate-3.3b",
                "files": [{
                    "path": "weights.bin",
                    "sha256": model_hash,
                    "size": 17
                }]
            }]
        }))
        .unwrap();
        std::fs::write(
            model_root.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "revision": CAT_TRANSLATE_REVISION,
                "files": { "weights.bin": model_hash }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            model_base.join(format!(".{CAT_TRANSLATE_REVISION}.receipt.json")),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "id": "cat_translate_3_3b",
                "version": CAT_TRANSLATE_REVISION,
                "manifest_sha256": sha256_bytes(&manifest),
                "installed_at": "fixture"
            }))
            .unwrap(),
        )
        .unwrap();
        (home, manifest, model_file)
    }

    fn response() -> serde_json::Value {
        serde_json::json!({
            "text": "翻訳済み {{POLARIS_SPAN_0}}",
            "input_tokens": 12,
            "output_tokens": 9,
            "finish_reason": "stop",
            "truncated": false,
            "model": CAT_TRANSLATE_MODEL,
            "revision": CAT_TRANSLATE_REVISION,
            "device": "cpu",
            "latency_ms": 42,
        })
    }

    fn preflight_response() -> serde_json::Value {
        serde_json::json!({
            "model": CAT_TRANSLATE_MODEL,
            "revision": CAT_TRANSLATE_REVISION,
            "device": "cpu",
            "packages": {
                "torch": "2.9.1",
                "transformers": "5.15.1",
                "tokenizers": "0.22.2",
                "safetensors": "0.8.0",
                "sentencepiece": "0.2.1",
            },
        })
    }

    #[test]
    fn directions_are_explicit_and_never_reversed() {
        assert_eq!(TranslationDirection::EnToJa.source_language(), "en");
        assert_eq!(TranslationDirection::EnToJa.target_language(), "ja");
        assert_eq!(TranslationDirection::JaToEn.source_language(), "ja");
        assert_eq!(TranslationDirection::JaToEn.target_language(), "en");
    }

    #[test]
    fn default_path_is_the_installer_managed_snapshot() {
        assert_eq!(
            LocalCatTranslate::default_model_path(Path::new("/Users/example")),
            Path::new("/Users/example/.polaris/models/cat-translate-3.3b")
                .join(CAT_TRANSLATE_REVISION)
        );
    }

    #[test]
    fn receipt_rejects_identity_drift_unknown_fields_and_truncation_lies() {
        assert!(LocalCatTranslate::parse_receipt(response()).is_ok());
        let mut drift = response();
        drift["revision"] = serde_json::json!("latest");
        assert!(LocalCatTranslate::parse_receipt(drift).is_err());
        let mut extra = response();
        extra["remote"] = serde_json::json!(false);
        assert!(LocalCatTranslate::parse_receipt(extra).is_err());
        let mut lie = response();
        lie["finish_reason"] = serde_json::json!("length");
        assert!(LocalCatTranslate::parse_receipt(lie).is_err());
        let mut missing_device = response();
        missing_device.as_object_mut().unwrap().remove("device");
        assert!(LocalCatTranslate::parse_receipt(missing_device).is_err());
        let mut unknown_device = response();
        unknown_device["device"] = serde_json::json!("cuda");
        assert!(LocalCatTranslate::parse_receipt(unknown_device).is_err());
    }

    #[test]
    fn preflight_requires_the_slow_tokenizer_package_identity() {
        assert!(LocalCatTranslate::validate_preflight(preflight_response()).is_ok());
        let mut missing = preflight_response();
        missing["packages"]
            .as_object_mut()
            .unwrap()
            .remove("sentencepiece");
        assert!(LocalCatTranslate::validate_preflight(missing).is_err());
        let mut unknown_device = preflight_response();
        unknown_device["device"] = serde_json::json!("cuda");
        assert!(LocalCatTranslate::validate_preflight(unknown_device).is_err());
    }

    #[test]
    fn bundled_helper_is_offline_pinned_and_does_not_offer_fallbacks() {
        let source = include_str!("../../../scripts/local_translation.py");
        assert!(source.contains("local_files_only=True"));
        assert!(source.contains("trust_remote_code=False"));
        assert!(source.contains("use_fast=False"));
        assert!(source.contains("do_sample=False"));
        assert!(!source.contains("requests."));
        assert!(!source.contains("subprocess"));
        assert!(source.contains(CAT_TRANSLATE_REVISION));
    }

    #[test]
    fn invalid_local_configuration_is_refused_before_spawn() {
        assert!(
            LocalCatTranslate::new(
                PathBuf::from("python3"),
                PathBuf::from("/helper"),
                PathBuf::from("/model"),
                PathBuf::from("/temp"),
                1,
                Duration::from_secs(1),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_model_requires_matching_receipt_manifest_and_bytes() {
        let root = tempfile::tempdir().unwrap();
        let (home, manifest, model_file) = managed_model_fixture(root.path());
        let (markers, directories) = verify_model_installation(&home, &manifest).unwrap();
        assert_eq!(markers.len(), 3);
        assert_eq!(
            directories.last().map(PathBuf::as_path),
            model_file.parent()
        );
        std::fs::write(model_file, b"changed model byte").unwrap();
        assert!(verify_model_installation(&home, &manifest).is_err());
    }

    #[test]
    fn managed_model_rejects_parent_paths_before_reading_payloads() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "dependencies": [{
                "id": "cat_translate_3_3b",
                "version": CAT_TRANSLATE_REVISION,
                "platform": "macos",
                "architecture": "universal",
                "destination": "models/cat-translate-3.3b",
                "files": [{
                    "path": "../weights.bin",
                    "sha256": "a".repeat(64),
                    "size": 1
                }]
            }]
        }))
        .unwrap();
        assert!(verify_model_installation(&home, &manifest).is_err());
    }
}
