//! Host-only detection of MCP servers already configured for Codex and Claude.
//! Sources are fixed files under the trusted home; values stay host-side and
//! are bound to a proposal by a per-owner HMAC. Nothing here saves or grants.

use polaris_mcp::registration::{Destination, Server};
#[cfg(unix)]
use polaris_mcp::{
    registration::{Capabilities, ProtocolProfile, Timeouts, normalize_http_url},
    store::Snapshot,
};
#[cfg(unix)]
use serde_json::Value;
#[cfg(any(unix, test))]
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{fs, io::Read, os::unix::fs::OpenOptionsExt};

#[cfg(unix)]
const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(unix)]
const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(unix)]
const BROAD_PREFIXES: &[&str] = &["/usr", "/usr/local", "/opt", "/opt/homebrew", "/opt/local"];
#[cfg(any(unix, test))]
const CREDENTIAL_FLAGS: &[&str] = &[
    "token",
    "secret",
    "pass",
    "api-key",
    "api_key",
    "apikey",
    "auth",
    "credential",
    "bearer",
];
#[cfg(any(unix, test))]
const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "glpat-",
    "AKIA",
    "AIza",
    "ya29.",
];
pub mod launcher;

#[cfg(unix)]
const LAUNCHERS: &[&str] = &[
    "npx", "npm", "pnpm", "yarn", "bunx", "bun", "uvx", "uv", "pipx", "deno", "docker",
];
#[cfg(unix)]
const SHIM_MARKERS: &[&str] = &["/shims/", "/.asdf/", "/.mise/", "/.volta/"];
#[cfg(unix)]
const RUNTIME_ROOTS: &[&str] = &["/bin", "/usr/bin", "/usr/lib", "/System/Library"];
#[cfg(unix)]
const ENV: &str = "/usr/bin/env";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    LauncherNotSupported,
    EnvironmentReference,
    CredentialInArguments,
    UnsupportedTransport,
    UnsupportedAuth,
    CommandNotFound,
    /// The command is written as a relative path. What it is relative to is
    /// not something this can decide: the config's own directory, the shell's
    /// working directory and the server's own are three different answers and
    /// picking one would run a file nobody named.
    CommandNotAbsolute,
    UnsupportedInstallLocation,
    AlreadyRegistered,
    /// The entry has no `command`, or it is not text.
    CommandMissing,
    /// An entry in `args` is not text.
    ArgumentNotText,
    /// The entry has no `url`, or it is not text.
    UrlMissing,
    /// The `url` is not one this can normalise.
    UrlNotUsable,
    Invalid,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LauncherNotSupported => "launcher_not_supported",
            Self::EnvironmentReference => "environment_reference",
            Self::CredentialInArguments => "credential_in_arguments",
            Self::UnsupportedTransport => "unsupported_transport",
            Self::UnsupportedAuth => "unsupported_auth",
            Self::CommandNotFound => "command_not_found",
            Self::CommandNotAbsolute => "command_not_absolute",
            Self::CommandMissing => "command_missing",
            Self::ArgumentNotText => "argument_not_text",
            Self::UrlMissing => "url_missing",
            Self::UrlNotUsable => "url_not_usable",
            Self::UnsupportedInstallLocation => "unsupported_install_location",
            Self::AlreadyRegistered => "already_registered",
            Self::Invalid => "invalid",
        }
    }
}

/// One secret for a proposal: where it was read and an HMAC of its value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBinding {
    pub reference: String,
    source: PathBuf,
    key_path: Vec<String>,
    mac: [u8; 32],
}

impl std::fmt::Debug for SecretBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretBinding({})", self.reference)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Proposal {
    pub server: Server,
    pub secrets: Vec<SecretBinding>,
    /// Revisions read at detection; the save compares both.
    pub expected_registry: u64,
    pub expected_secrets: u64,
    /// Replaces the saved server with the same ID.
    pub replaces: bool,
}

impl std::fmt::Debug for Proposal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Proposal({}, {:?})", self.server.server_id, self.secrets)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detected {
    /// Opaque, random per detection; never derived from the source.
    pub id: String,
    pub name: String,
    pub transport: &'static str,
    pub reason: Option<Reason>,
    pub proposal: Option<Proposal>,
}

pub struct Detector {
    home: PathBuf,
    path: Vec<PathBuf>,
    key: [u8; 32],
}

/// An account-bound fetch snapshot. Only the detector can construct it; it
/// carries neither a serialized command request nor permission to execute.
#[cfg(unix)]
pub struct MaterialisePlan {
    home: PathBuf,
    source_hash: [u8; 32],
    command: String,
    arguments: Vec<String>,
    directory: PathBuf,
    runtime: PathBuf,
    package: String,
    version: Option<String>,
}

#[cfg(unix)]
impl MaterialisePlan {
    /// The exact download request, interpreter and managed destination shown
    /// before execution. Secret-bearing arguments are refused during prepare.
    pub fn description(&self) -> String {
        format!(
            "package: {}\nversion: {}\ninterpreter: {}\nStorage: {}\nDownloaded from the network once. Installation scripts are not executed.\nRegistration is confirmed separately. Rewind cannot undo this download.",
            self.package,
            self.version
                .as_deref()
                .unwrap_or("Resolved and pinned during download"),
            self.runtime.display(),
            self.directory.display(),
        )
    }
}

/// Detection target and the registry revision that names new secret references.
#[cfg(unix)]
struct Scope<'a> {
    project: &'a str,
    revision: u64,
}

/// Registers the fixed sources before any model-visible read in this process.
#[cfg(unix)]
pub fn protect_sources(home: &Path) -> Result<(), polaris_mcp::Error> {
    // Register every source even when one of them cannot be registered.
    let results = [home.join(".codex/config.toml"), home.join(".claude.json")]
        .map(|path| polaris_auth::protection::protect_path(&path));
    if results.iter().any(Result::is_err) {
        return Err(polaris_mcp::Error::Protection);
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn protect_sources(_home: &Path) -> Result<(), polaris_mcp::Error> {
    Err(polaris_mcp::Error::Protection)
}

#[cfg(unix)]
fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .expect("system randomness");
    bytes
}

#[cfg(any(unix, test))]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(any(unix, test))]
fn hmac(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    // RFC 2104 with a 64-byte block: the 32-byte key padded with zeros.
    for byte in key.iter().chain(&[0; 32]) {
        inner.update([byte ^ 0x36]);
        outer.update([byte ^ 0x5c]);
    }
    inner.update(message);
    outer.update(inner.finalize());
    outer.finalize().into()
}

#[cfg(any(unix, test))]
fn sanitized_id(name: &str, identity: &str) -> String {
    let hash = hex(&Sha256::digest(identity.as_bytes()));
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(47)
        .collect();
    format!("{base}-{}", &hash[..16])
}

/// Credentials cannot be stored as plain registry fields. Refuse flag/value
/// pairs, credential-named header lines, known token prefixes in any token,
/// userinfo and long opaque values.
#[cfg(any(unix, test))]
fn credential_like(args: &[String]) -> bool {
    let opaque = |text: &str| {
        text.len() >= 32
            && !text.contains('/')
            && text.bytes().any(|b| b.is_ascii_digit())
            && text.bytes().any(|b| b.is_ascii_alphabetic())
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+=".contains(&b))
    };
    args.iter().enumerate().any(|(i, arg)| {
        let lower = arg.to_lowercase();
        let (flag, inline) = match lower.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (lower.as_str(), None),
        };
        let named = flag.starts_with('-')
            && CREDENTIAL_FLAGS
                .iter()
                .any(|marker| flag.trim_start_matches('-').contains(marker));
        named && (inline.is_some() || args.get(i + 1).is_some_and(|next| !next.starts_with('-')))
            || lower.split_once(':').is_some_and(|(name, _)| {
                let name = name.trim().trim_start_matches('-');
                ["key", "cookie"]
                    .iter()
                    .chain(CREDENTIAL_FLAGS)
                    .any(|marker| name.contains(marker))
            })
            || lower.contains("bearer ")
            || arg
                .split(|c: char| c.is_whitespace() || "=:/,".contains(c))
                .any(|token| {
                    TOKEN_PREFIXES
                        .iter()
                        .any(|prefix| token.starts_with(prefix))
                })
            || arg.split("://").nth(1).is_some_and(|rest| {
                rest.split('/')
                    .next()
                    .is_some_and(|authority| authority.contains('@'))
            })
            || opaque(arg)
    })
}

#[cfg(any(unix, test))]
fn url_credential_like(url: &str) -> bool {
    credential_like(&[url.to_owned()])
        || url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, path)| {
                path.split(['/', '.']).any(|segment| {
                    segment.len() >= 32
                        && segment
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_+=".contains(&b))
                })
            })
}

/// A regular, bounded file opened without following the final link.
#[cfg(unix)]
fn open_regular(path: &Path) -> Result<fs::File, Reason> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| Reason::CommandNotFound)?;
    let metadata = file.metadata().map_err(|_| Reason::CommandNotFound)?;
    if !metadata.is_file() || metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(Reason::Invalid);
    }
    Ok(file)
}

#[cfg(unix)]
fn file_digest(path: &Path) -> Result<String, Reason> {
    let mut hasher = Sha256::new();
    std::io::copy(
        &mut open_regular(path)?.take(MAX_EXECUTABLE_BYTES),
        &mut hasher,
    )
    .map_err(|_| Reason::Invalid)?;
    Ok(hex(&hasher.finalize()))
}

#[cfg(unix)]
impl Detector {
    pub fn new(home: PathBuf, path: Vec<PathBuf>) -> Self {
        let _ = protect_sources(&home);
        Self {
            home,
            path: path.into_iter().filter(|dir| dir.is_absolute()).collect(),
            key: random(),
        }
    }

    fn sources(&self) -> [PathBuf; 2] {
        [
            self.home.join(".codex/config.toml"),
            self.home.join(".claude.json"),
        ]
    }

    /// Protect before reading; open without following links; bounded size.
    fn read(path: &Path) -> Option<String> {
        polaris_auth::protection::protect_path(path).ok()?;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
            return None;
        }
        let mut body = String::new();
        file.take(MAX_SOURCE_BYTES).read_to_string(&mut body).ok()?;
        Some(body)
    }

    /// Fetches the package a launcher-shaped entry names, once, and pins it.
    ///
    /// After this, the same entry detects as an ordinary local executable and
    /// registers through the path every other server takes. **Nothing here
    /// registers anything**: it prepares, and the person still approves the
    /// registration afterwards with the digest and the resolved version in
    /// front of them.
    ///
    /// Only the Codex source is read. The Claude one holds the same shape and
    /// can be added when something needs it; two readers for one feature
    /// before either is used is a guess about which one matters.
    pub fn materialise(
        &self,
        name: &str,
        runtime: Option<&Path>,
    ) -> Result<launcher::Pinned, launcher::Refused> {
        self.materialise_with_output(name, runtime, false)
    }

    /// TUI hosts keep package-manager output out of their terminal renderer.
    /// Fetching, pin validation and the separate registration boundary are shared.
    pub fn materialise_quiet(
        &self,
        name: &str,
        runtime: Option<&Path>,
    ) -> Result<launcher::Pinned, launcher::Refused> {
        self.materialise_with_output(name, runtime, true)
    }

    /// Binds the existing launcher parser to the source and runtime before a
    /// TUI asks for approval. No package is downloaded by this preparation.
    pub fn prepare_materialise(
        &self,
        name: &str,
        runtime: Option<&Path>,
    ) -> Result<MaterialisePlan, launcher::Refused> {
        let [source, _] = self.sources();
        let body = Self::read(&source).ok_or(launcher::Refused::NotALauncher)?;
        let document =
            toml::from_str::<toml::Value>(&body).map_err(|_| launcher::Refused::NotALauncher)?;
        let entry = document
            .get("mcp_servers")
            .and_then(|servers| servers.get(name))
            .ok_or(launcher::Refused::NotALauncher)?;
        let command = entry
            .get("command")
            .and_then(toml::Value::as_str)
            .ok_or(launcher::Refused::NotALauncher)?
            .to_owned();
        let arguments: Vec<String> = entry
            .get("args")
            .and_then(toml::Value::as_array)
            .ok_or(launcher::Refused::NotALauncher)?
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or(launcher::Refused::NotALauncher)?;
        if credential_like(&arguments) || command.chars().any(char::is_control) {
            return Err(launcher::Refused::NotALauncher);
        }
        let request =
            launcher::request(&command, &arguments).ok_or(launcher::Refused::NotALauncher)?;
        let runtime = match runtime {
            Some(runtime) => runtime.to_owned(),
            None => launcher::default_runtime(request.manager)
                .ok_or(launcher::Refused::RuntimeNotPinned)?,
        };
        if !runtime.is_absolute() || !runtime.is_file() {
            return Err(launcher::Refused::RuntimeNotPinned);
        }
        let server_id = sanitized_id(
            name,
            &format!("{}\n{}", source.display(), ["mcp_servers", name].join("\n")),
        );
        Ok(MaterialisePlan {
            home: self.home.clone(),
            source_hash: Sha256::digest(body.as_bytes()).into(),
            command,
            arguments,
            directory: self.home.join(".polaris/mcp/servers").join(server_id),
            runtime,
            package: request.package,
            version: request.version,
        })
    }

    /// Rechecks the source, then executes only the approved snapshot. A source
    /// change cannot substitute another package after the approval card.
    pub fn materialise_prepared_quiet(
        &self,
        plan: MaterialisePlan,
    ) -> Result<launcher::Pinned, launcher::Refused> {
        let [source, _] = self.sources();
        let body = Self::read(&source).ok_or(launcher::Refused::NotALauncher)?;
        let source_hash: [u8; 32] = Sha256::digest(body.as_bytes()).into();
        if plan.home != self.home || source_hash != plan.source_hash {
            return Err(launcher::Refused::NotALauncher);
        }
        // Reuse the store's descriptor-relative directory owner before the
        // downloader receives a path. It rejects symlinked managed roots.
        let store =
            polaris_mcp::store::Store::open(&self.home).map_err(|_| launcher::Refused::NotSaved)?;
        let server_id = plan
            .directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(launcher::Refused::NotSaved)?;
        if store
            .server_directory(server_id)
            .map_err(|_| launcher::Refused::NotSaved)?
            != plan.directory
        {
            return Err(launcher::Refused::NotSaved);
        }
        launcher::materialise_quiet(
            &plan.directory,
            &plan.command,
            &plan.arguments,
            Some(&plan.runtime),
        )
    }

    fn materialise_with_output(
        &self,
        name: &str,
        runtime: Option<&Path>,
        quiet: bool,
    ) -> Result<launcher::Pinned, launcher::Refused> {
        let [codex, _] = self.sources();
        let body = Self::read(&codex).ok_or(launcher::Refused::NotALauncher)?;
        let document =
            toml::from_str::<toml::Value>(&body).map_err(|_| launcher::Refused::NotALauncher)?;
        let entry = document
            .get("mcp_servers")
            .and_then(|held| held.get(name))
            .ok_or(launcher::Refused::NotALauncher)?;
        let command = entry
            .get("command")
            .and_then(toml::Value::as_str)
            .ok_or(launcher::Refused::NotALauncher)?;
        let args: Vec<String> = entry
            .get("args")
            .and_then(toml::Value::as_array)
            .map(|held| {
                held.iter()
                    .filter_map(|held| held.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let server_id = sanitized_id(
            name,
            &format!("{}\n{}", codex.display(), ["mcp_servers", name].join("\n")),
        );
        let directory = self.home.join(".polaris/mcp/servers").join(&server_id);
        if quiet {
            launcher::materialise_quiet(&directory, command, &args, runtime)
        } else {
            launcher::materialise(&directory, command, &args, runtime)
        }
    }

    /// `current` is the saved state proposals are bound to.
    pub fn detect(&self, project: &str, current: &Snapshot) -> Vec<Detected> {
        let scope = Scope {
            project,
            revision: current.registry.as_ref().map_or(0, |r| r.revision),
        };
        let project = &scope;
        let mut found = Vec::new();
        let [codex, claude] = self.sources();
        if let Some(body) = Self::read(&codex)
            && let Ok(document) = toml::from_str::<toml::Value>(&body)
            && let Some(servers) = document.get("mcp_servers").and_then(|v| v.as_table())
        {
            for (name, entry) in servers {
                let entry = serde_json::to_value(entry).unwrap_or(Value::Null);
                found.push(self.codex(&codex, name, &entry, project));
            }
        }
        if let Some(body) = Self::read(&claude)
            && let Ok(document) = serde_json::from_str::<Value>(&body)
        {
            let mut scopes = vec![(vec!["mcpServers".to_owned()], &document["mcpServers"])];
            if let Some(projects) = document["projects"].as_object() {
                for (key, value) in projects {
                    scopes.push((
                        vec!["projects".into(), key.clone(), "mcpServers".into()],
                        &value["mcpServers"],
                    ));
                }
            }
            for (prefix, servers) in scopes {
                for (name, entry) in servers.as_object().into_iter().flatten() {
                    let mut path = prefix.clone();
                    path.push(name.clone());
                    found.push(self.claude(&claude, name, path, entry, project));
                }
            }
        }
        for detected in &mut found {
            self.reconcile(detected, current);
        }
        found
    }

    /// A matching saved server is reported instead of proposed. A differing one
    /// is replaced, keeping the user's projects and capability choices.
    fn reconcile(&self, detected: &mut Detected, current: &Snapshot) {
        let Some(proposal) = &mut detected.proposal else {
            return;
        };
        proposal.expected_registry = current.registry.as_ref().map_or(0, |r| r.revision);
        proposal.expected_secrets = current.secrets.as_ref().map_or(0, |s| s.revision());
        let Some(existing) = current.registry.as_ref().and_then(|registry| {
            registry
                .servers
                .iter()
                .find(|s| s.server_id == proposal.server.server_id)
        }) else {
            return;
        };
        let server = &mut proposal.server;
        server
            .enabled_projects
            .extend(existing.enabled_projects.iter().cloned());
        server.capabilities = existing.capabilities;
        let shape = |server: &Server| {
            let mut server = server.clone();
            let references = match &mut server.transport {
                Destination::Stdio { secret_env, .. } => secret_env,
                Destination::StreamableHttp { secret_headers, .. } => secret_headers,
            };
            let references = std::mem::take(references);
            (server, references)
        };
        let (saved, saved_references) = shape(existing);
        let (proposed, proposed_references) = shape(server);
        let same_values = saved_references.len() == proposed_references.len()
            && saved_references.iter().all(|(name, reference)| {
                let stored = current
                    .secrets
                    .as_ref()
                    .and_then(|secrets| secrets.resolve(reference).ok());
                let binding = proposed_references
                    .get(name)
                    .and_then(|r| proposal.secrets.iter().find(|b| &b.reference == r));
                matches!((stored, binding), (Some(value), Some(binding))
                    if hmac(&self.key, value.as_bytes()) == binding.mac)
            });
        if saved == proposed && same_values {
            detected.proposal = None;
            detected.reason = Some(Reason::AlreadyRegistered);
        } else {
            proposal.replaces = true;
        }
    }

    fn candidate(
        name: &str,
        transport: &'static str,
        result: Result<Proposal, Reason>,
    ) -> Detected {
        let (reason, proposal) = match result {
            Ok(proposal) => (None, Some(proposal)),
            Err(reason) => (Some(reason), None),
        };
        Detected {
            id: hex(&random::<16>()),
            name: name.into(),
            transport,
            reason,
            proposal,
        }
    }

    fn codex(&self, source: &Path, name: &str, entry: &Value, project: &Scope) -> Detected {
        let key = |field: &str, sub: &str| {
            vec![
                "mcp_servers".to_owned(),
                name.into(),
                field.into(),
                sub.into(),
            ]
        };
        let base = || vec!["mcp_servers".to_owned(), name.to_owned()];
        if entry.get("url").is_some() {
            let entry_path = base();
            let result = if entry
                .get("auth")
                .and_then(Value::as_str)
                .is_some_and(|a| a != "none")
                || entry.get("oauth").is_some()
            {
                Err(Reason::UnsupportedAuth)
            } else if entry.get("bearer_token_env_var").is_some()
                || entry.get("env_http_headers").is_some()
            {
                Err(Reason::EnvironmentReference)
            } else {
                let mut headers: Vec<(String, Vec<String>)> = entry["http_headers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(header, _)| (header.to_lowercase(), key("http_headers", header)))
                    .collect();
                if entry.get("bearer_token").is_some() {
                    let mut path = base();
                    path.push("bearer_token".into());
                    headers.push(("authorization".into(), path));
                }
                self.http(
                    source,
                    &entry_path,
                    name,
                    entry["url"].as_str(),
                    headers,
                    project,
                )
            };
            return Self::candidate(name, "streamable_http", result);
        }
        let result = if entry.get("env_vars").is_some() {
            Err(Reason::EnvironmentReference)
        } else {
            let env = entry["env"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(variable, _)| (variable.clone(), key("env", variable)))
                .collect();
            self.stdio(source, &base(), name, entry, env, project)
        };
        Self::candidate(name, "stdio", result)
    }

    fn claude(
        &self,
        source: &Path,
        name: &str,
        path: Vec<String>,
        entry: &Value,
        project: &Scope,
    ) -> Detected {
        let field = |field: &str, sub: &str| {
            let mut key = path.clone();
            key.extend([field.to_owned(), sub.to_owned()]);
            key
        };
        match entry["type"].as_str().unwrap_or("stdio") {
            "http" => {
                let headers = entry["headers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(header, _)| (header.to_lowercase(), field("headers", header)))
                    .collect();
                let result =
                    self.http(source, &path, name, entry["url"].as_str(), headers, project);
                Self::candidate(name, "streamable_http", result)
            }
            "stdio" => {
                let env = entry["env"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(variable, _)| (variable.clone(), field("env", variable)))
                    .collect();
                let result = self.stdio(source, &path, name, entry, env, project);
                Self::candidate(name, "stdio", result)
            }
            _ => Self::candidate(name, "unsupported", Err(Reason::UnsupportedTransport)),
        }
    }

    /// Reference names include the registry revision, so a later
    /// re-registration never writes over a live reference.
    fn binding(
        &self,
        source: &Path,
        server_id: &str,
        name: &str,
        key_path: Vec<String>,
        revision: u64,
    ) -> Result<SecretBinding, Reason> {
        let value = self.value_at(source, &key_path).ok_or(Reason::Invalid)?;
        if value.contains("${") {
            return Err(Reason::EnvironmentReference);
        }
        let reference = sanitized_id(
            &server_id[..server_id.len().min(47)],
            &format!(
                "{}\n{}\n{name}\n{revision}",
                source.display(),
                key_path.join("\n")
            ),
        );
        Ok(SecretBinding {
            reference,
            source: source.into(),
            mac: hmac(&self.key, value.as_bytes()),
            key_path,
        })
    }

    fn value_at(&self, source: &Path, key_path: &[String]) -> Option<String> {
        let body = Self::read(source)?;
        let document: Value = if source.extension().is_some_and(|e| e == "toml") {
            serde_json::to_value(toml::from_str::<toml::Value>(&body).ok()?).ok()?
        } else {
            serde_json::from_str(&body).ok()?
        };
        let mut current = &document;
        for key in key_path {
            current = current.get(key)?;
        }
        let value = current.as_str()?;
        let value = if key_path.last().is_some_and(|k| k == "bearer_token") {
            format!("Bearer {value}")
        } else {
            value.to_owned()
        };
        (!value.is_empty() && !value.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n')))
            .then_some(value)
    }

    /// A known app's protocol and sandbox profiles apply only through the
    /// approval card, which shows both.
    fn server(project: &str, transport: Destination) -> Server {
        let mut server = Server {
            server_id: String::new(),
            enabled_projects: [project.to_owned()].into(),
            transport,
            protocol_profile: ProtocolProfile::default(),
            capabilities: Capabilities {
                catalog_metadata: true,
                resource_read: true,
                prompt_selection: true,
                tool_call: true,
            },
            timeouts: Timeouts::default(),
        };
        if let Some(app) = super::apps::find(&server) {
            server.protocol_profile = app.protocol_profile;
            if let (
                Destination::Stdio {
                    sandbox_profile, ..
                },
                Some(profile),
            ) = (&mut server.transport, app.sandbox_profile)
            {
                *sandbox_profile = profile.into();
            }
        }
        server
    }

    fn http(
        &self,
        source: &Path,
        entry_path: &[String],
        name: &str,
        url: Option<&str>,
        headers: Vec<(String, Vec<String>)>,
        project: &Scope,
    ) -> Result<Proposal, Reason> {
        let url = url.ok_or(Reason::UrlMissing)?;
        if url.contains("${") {
            return Err(Reason::EnvironmentReference);
        }
        if url_credential_like(url) {
            return Err(Reason::CredentialInArguments);
        }
        let url = normalize_http_url(url).map_err(|_| Reason::UrlNotUsable)?;
        let server_id = sanitized_id(
            name,
            &format!("{}\n{}", source.display(), entry_path.join("\n")),
        );
        let mut secrets = Vec::new();
        let mut secret_headers = BTreeMap::new();
        for (header, key_path) in headers {
            let binding = self.binding(source, &server_id, &header, key_path, project.revision)?;
            secret_headers.insert(header, binding.reference.clone());
            secrets.push(binding);
        }
        let mut server = Self::server(
            project.project,
            Destination::StreamableHttp {
                url,
                secret_headers,
            },
        );
        server.server_id = server_id;
        server.validate().map_err(|_| Reason::Invalid)?;
        Ok(Proposal {
            server,
            secrets,
            expected_registry: 0,
            expected_secrets: 0,
            replaces: false,
        })
    }

    fn resolve(&self, command: &str) -> Result<PathBuf, Reason> {
        let launcher = |path: &Path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| LAUNCHERS.contains(&name))
                || SHIM_MARKERS
                    .iter()
                    .any(|marker| path.to_string_lossy().contains(marker))
        };
        if launcher(Path::new(command)) {
            return Err(Reason::LauncherNotSupported);
        }
        let candidate = if command.contains('/') {
            Path::new(command)
                .is_absolute()
                .then(|| PathBuf::from(command))
                .ok_or(Reason::CommandNotAbsolute)?
        } else {
            self.path
                .iter()
                .map(|dir| dir.join(command))
                .find(|p| p.is_file())
                .ok_or(Reason::CommandNotFound)?
        };
        let resolved = candidate
            .canonicalize()
            .map_err(|_| Reason::CommandNotFound)?;
        if launcher(&candidate) || launcher(&resolved) {
            return Err(Reason::LauncherNotSupported);
        }
        Ok(resolved)
    }

    /// The install tree for a pinned file: a `bin` directory's parent unless that
    /// is shared, where `bin` itself is used. Home, its ancestors and children,
    /// Application Support and shared prefixes are never offered.
    fn install_root(&self, path: &Path) -> Result<Option<PathBuf>, Reason> {
        if RUNTIME_ROOTS.iter().any(|root| path.starts_with(root)) {
            return Ok(None);
        }
        let shared = |dir: &Path| {
            self.home.starts_with(dir)
                || dir.parent() == Some(self.home.as_path())
                || dir == self.home.join("Library/Application Support")
                || BROAD_PREFIXES.iter().any(|broad| dir == Path::new(broad))
        };
        let parent = path.parent().ok_or(Reason::Invalid)?;
        let root = parent
            .file_name()
            .is_some_and(|name| name == "bin")
            .then(|| parent.parent())
            .flatten()
            .filter(|grand| !shared(grand))
            .unwrap_or(parent);
        if shared(root) {
            return Err(Reason::UnsupportedInstallLocation);
        }
        Ok(Some(root.to_owned()))
    }

    fn stdio(
        &self,
        source: &Path,
        entry_path: &[String],
        name: &str,
        entry: &Value,
        env: Vec<(String, Vec<String>)>,
        project: &Scope,
    ) -> Result<Proposal, Reason> {
        let mut raw = entry["command"]
            .as_str()
            .ok_or(Reason::CommandMissing)?
            .to_owned();
        let mut args: Vec<String> = entry["args"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| a.as_str().map(str::to_owned).ok_or(Reason::ArgumentNotText))
            .collect::<Result<_, _>>()?;
        if raw.contains("${") || args.iter().any(|a| a.contains("${")) {
            return Err(Reason::EnvironmentReference);
        }
        // A launcher fetches ateach launch, so registration refuses it: the
        // digest would be the launcher's. It stops being a launcher once the
        // package has been fetched into the directory polaris keeps for this
        // server — from there it is an ordinary local executable, and every
        // check below applies to it unchanged.
        let server_id = sanitized_id(
            name,
            &format!("{}\n{}", source.display(), entry_path.join("\n")),
        );
        let directory = self.home.join(".polaris/mcp/servers").join(&server_id);
        let mut pinned_entry = None;
        if let Some(request) = launcher::request(&raw, &args)
            && let Some(held) = launcher::pinned(&directory, &request)
        {
            let mut rest = request.arguments;
            args = vec![held.entry.to_str().ok_or(Reason::Invalid)?.to_owned()];
            args.append(&mut rest);
            raw = held.runtime.to_str().ok_or(Reason::Invalid)?.to_owned();
            pinned_entry = Some(held.entry);
        }
        let original = self.resolve(&raw)?;
        if original == Path::new(ENV) {
            return Err(Reason::Invalid);
        }
        if credential_like(&args) {
            return Err(Reason::CredentialInArguments);
        }
        // Run a script's interpreter directly so the pinned binaries are what run.
        let mut command = original.clone();
        let mut script = None;
        let mut head = [0; 512];
        let read = open_regular(&original)?.read(&mut head).unwrap_or(0);
        if head[..read].starts_with(b"#!") {
            let line = head[2..read]
                .split(|b| *b == b'\n')
                .next()
                .filter(|_| head[..read].contains(&b'\n'))
                .ok_or(Reason::Invalid)?;
            let line = std::str::from_utf8(line).map_err(|_| Reason::Invalid)?;
            let mut words = line.trim_end_matches('\r').split_whitespace();
            command = self.resolve(words.next().ok_or(Reason::Invalid)?)?;
            if command == Path::new(ENV) {
                let target = words.next().ok_or(Reason::Invalid)?;
                if target.starts_with('-') {
                    return Err(Reason::Invalid);
                }
                command = self.resolve(target)?;
                if command == Path::new(ENV) {
                    return Err(Reason::Invalid);
                }
            }
            let mut prefix: Vec<String> = words.map(str::to_owned).collect();
            prefix.push(original.to_str().ok_or(Reason::Invalid)?.into());
            prefix.extend(args);
            args = prefix;
            script = Some(original);
        }
        // An interpreter's digest is the same for every server that shares it:
        // 24 pinned servers showed one identical SHA-256 on 24 approval cards,
        // node's, and said nothing about the code each would run. What differs
        // is the entry the pin names, and it was only named in `args` — no
        // digest on the card, nothing re-checked at launch, so a later write
        // into `node_modules` would have been launched with nobody seeing it.
        // It is already the first argument, which is what `script` means here.
        if script.is_none() {
            script = pinned_entry;
        }
        let executable_sha256 = Some(file_digest(&command)?);
        let script_sha256 = script.as_deref().map(file_digest).transpose()?;
        let mut readable_roots = Vec::new();
        for path in [Some(&command), script.as_ref()].into_iter().flatten() {
            // The server's own directory is the sandbox workspace and already
            // readable. Offering it as a root is worse than redundant:
            // admission refuses a root that overlaps the workspace, and would
            // refuse the whole server for it.
            if path.starts_with(&directory) {
                continue;
            }
            let Some(root) = self.install_root(path)? else {
                continue;
            };
            // Ask the question a run will ask, instead of proposing a root it
            // refuses later with nothing left to act on. An install tree that
            // is not admissible falls back to the file itself: `/usr/local/bin`
            // holds a world-writable binary and symlinks that leave the tree,
            // so the whole root is refused, while `node` — the one file that
            // has to be read — is fine alone. The excluded roots are left out
            // here because they belong to the run, not to this proposal; a run
            // applies its own, and this call can only narrow what is offered.
            //
            // A program that needs more than itself now fails at launch rather
            // than at registration. That is the cost of not handing over a
            // directory nobody vouched for, and it is the cheaper of the two.
            let root = match crate::mcp::runtime::resources::admit_read_root(
                &root,
                &self.home,
                &directory,
                &[],
            ) {
                Ok(()) => root,
                Err(_) => path.clone(),
            };
            if !readable_roots.contains(&root) {
                readable_roots.push(root);
            }
        }
        let mut secrets = Vec::new();
        let mut secret_env = BTreeMap::new();
        for (variable, key_path) in env {
            let binding =
                self.binding(source, &server_id, &variable, key_path, project.revision)?;
            secret_env.insert(variable, binding.reference.clone());
            secrets.push(binding);
        }
        let transport = Destination::Stdio {
            command,
            args,
            cwd: directory,
            sandbox_profile: "read-only".into(),
            secret_env,
            readable_roots,
            executable_sha256,
            script,
            script_sha256,
        };
        let mut server = Self::server(project.project, transport);
        server.server_id = server_id;
        server.validate().map_err(|_| Reason::Invalid)?;
        Ok(Proposal {
            server,
            secrets,
            expected_registry: 0,
            expected_secrets: 0,
            replaces: false,
        })
    }

    /// Values for an approved proposal, only while each still matches its HMAC.
    pub fn secret_values(
        &self,
        proposal: &Proposal,
    ) -> Result<Vec<(String, polaris_mcp::store::SecretValue)>, polaris_mcp::Error> {
        proposal
            .secrets
            .iter()
            .map(|binding| {
                let value = self
                    .value_at(&binding.source, &binding.key_path)
                    .filter(|value| hmac(&self.key, value.as_bytes()) == binding.mac)
                    .ok_or(polaris_mcp::Error::Auth)?;
                Ok((
                    binding.reference.clone(),
                    polaris_mcp::store::SecretValue::new(value),
                ))
            })
            .collect()
    }
}

mod register;
mod tool;
pub use register::Admission;
pub use tool::{Authorization, HOST_PATH, SetupTool};

#[cfg(test)]
mod data_tests;
#[cfg(all(test, unix))]
mod materialise_tests;
#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;
