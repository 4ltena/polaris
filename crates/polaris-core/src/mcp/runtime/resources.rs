//! Trusted startup resources, shared with the factory until native cleanup ends.
use super::*;
use crate::isolated_workspace::{Limits, RegisteredDirectory};
use polaris_mcp::store::Snapshot;
use polaris_sandbox::SandboxMode;
use std::{
    fs::{self, File, Metadata, OpenOptions, Permissions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn prepare_scratch(
    parent: &Path,
    excluded_roots: &[PathBuf],
    readable_roots: &[PathBuf],
) -> Result<TempDir, polaris_mcp::Error> {
    let parent = parent
        .canonicalize()
        .map_err(|_| polaris_mcp::Error::Protection)?;
    // Check BEFORE creating anything. A caller-selected TMPDIR must not turn
    // source/store/runtime access into an extra write grant.
    if excluded_roots
        .iter()
        .chain(readable_roots)
        .any(|root| parent.starts_with(root))
        || polaris_tools::path_policy::is_denied(&parent)
    {
        return Err(polaris_mcp::Error::Protection);
    }
    polaris_auth::protection::with_protected_paths(|registered| {
        let secrets = crate::isolated_workspace::RegisteredSecrets::from_auth_registry(registered)
            .map_err(|_| polaris_mcp::Error::Protection)?;
        let metadata = parent
            .symlink_metadata()
            .map_err(|_| polaris_mcp::Error::Protection)?;
        if !metadata.is_dir()
            || secrets.denies(&parent, &metadata)
            || registered
                .paths()
                .iter()
                .any(|path| path.parent() == Some(parent.as_path()))
        {
            return Err(polaris_mcp::Error::Protection);
        }
        tempfile::Builder::new()
            .prefix("polaris-mcp-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(&parent)
            .map_err(|_| polaris_mcp::Error::Protection)
    })
    .map_err(|_| polaris_mcp::Error::Protection)?
}

pub(super) struct StdioResources {
    cwd: RegisteredDirectory,
    // Pinned files: the executable and, for interpreted servers, its script.
    pinned: Vec<(PathBuf, File, Metadata)>,
    _scratch: TempDir,
    scratch_directories: Vec<(PathBuf, File)>,
}

impl StdioResources {
    pub(super) fn revalidate(&self) -> Result<(), polaris_mcp::Error> {
        let check = || -> std::io::Result<bool> {
            let same = |a: &Metadata, b: &Metadata| {
                (
                    a.dev(),
                    a.ino(),
                    a.mode(),
                    a.uid(),
                    a.nlink(),
                    a.len(),
                    a.mtime(),
                    a.mtime_nsec(),
                    a.ctime(),
                    a.ctime_nsec(),
                ) == (
                    b.dev(),
                    b.ino(),
                    b.mode(),
                    b.uid(),
                    b.nlink(),
                    b.len(),
                    b.mtime(),
                    b.mtime_nsec(),
                    b.ctime(),
                    b.ctime_nsec(),
                )
            };
            for (path, file, metadata) in &self.pinned {
                if path.canonicalize()? != *path
                    || !same(&path.symlink_metadata()?, metadata)
                    || !same(&file.metadata()?, metadata)
                {
                    return Ok(false);
                }
            }
            for (path, pinned) in &self.scratch_directories {
                let now = path.symlink_metadata()?;
                let before = pinned.metadata()?;
                if path.canonicalize()? != *path
                    || !now.is_dir()
                    || (now.dev(), now.ino()) != (before.dev(), before.ino())
                    || now.uid() != unsafe { libc::geteuid() }
                    || now.mode() & 0o7077 != 0
                {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        if !check().map_err(|_| polaris_mcp::Error::Protection)? {
            return Err(polaris_mcp::Error::Protection);
        }
        self.cwd
            .revalidate()
            .map_err(|_| polaris_mcp::Error::Protection)
    }
}

const MAX_ROOT_ENTRIES: usize = 65_536;
const MAX_ROOT_DEPTH: usize = 64;
const MAX_PINNED_BYTES: u64 = 512 * 1024 * 1024;
const BROAD_ROOTS: &[&str] = &["/usr", "/usr/local", "/opt", "/opt/homebrew", "/opt/local"];

/// An install tree exposed read-only to one server. It is never home, a home
/// ancestor or direct child, a shared prefix, or overlapping the cwd, source or
/// store. Every entry is owned by root or this user and not group/other-writable,
/// links stay inside, and no protected path, identity or denied name is beneath.
pub(crate) fn admit_read_root(
    root: &Path,
    home: &Path,
    cwd: &Path,
    excluded_roots: &[PathBuf],
) -> Result<(), polaris_mcp::Error> {
    let denied = || polaris_mcp::Error::Protection;
    if root.canonicalize().map_err(|_| denied())? != root
        || root.parent().is_none()
        || BROAD_ROOTS.iter().any(|broad| root == Path::new(broad))
        || home.starts_with(root)
        || root.parent() == Some(home)
        || root == home.join("Library/Application Support")
        || excluded_roots
            .iter()
            .map(PathBuf::as_path)
            .chain([cwd])
            .any(|other| other.starts_with(root) || root.starts_with(other))
    {
        return Err(denied());
    }
    // Copy the protection snapshot; never walk while holding the auth lock.
    let (paths, identities) = polaris_auth::protection::with_protected_paths(|registered| {
        (registered.paths().clone(), registered.identities().clone())
    })
    .map_err(|_| denied())?;
    if paths.iter().any(|path| path.starts_with(root)) {
        return Err(denied());
    }
    // SAFETY: geteuid has no arguments or memory effects.
    let euid = unsafe { libc::geteuid() };
    let mut stack = vec![(root.to_path_buf(), 0)];
    let mut seen = 0;
    while let Some((path, depth)) = stack.pop() {
        seen += 1;
        if depth > MAX_ROOT_DEPTH {
            return Err(polaris_mcp::Error::Limit);
        }
        let metadata = path.symlink_metadata().map_err(|_| denied())?;
        if polaris_tools::path_policy::is_denied(&path)
            || identities.contains(&polaris_auth::protection::ProtectedIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            })
            || (metadata.uid() != 0 && metadata.uid() != euid)
        {
            return Err(denied());
        }
        if metadata.file_type().is_symlink() {
            if !path
                .canonicalize()
                .is_ok_and(|target| target.starts_with(root))
            {
                return Err(denied());
            }
            continue;
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(denied());
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|_| denied())? {
                if seen + stack.len() >= MAX_ROOT_ENTRIES {
                    return Err(polaris_mcp::Error::Limit);
                }
                stack.push((entry.map_err(|_| denied())?.path(), depth + 1));
            }
        }
    }
    Ok(())
}

/// Open without following links, require a bounded regular file, and compare
/// the digest read from that same descriptor.
fn pinned_file(
    path: &Path,
    expected: Option<&String>,
) -> Result<(PathBuf, File, Metadata), polaris_mcp::Error> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let denied = || polaris_mcp::Error::Protection;
    if path.canonicalize().map_err(|_| denied())? != path {
        return Err(denied());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| denied())?;
    let metadata = file.metadata().map_err(|_| denied())?;
    if !metadata.is_file() || metadata.len() > MAX_PINNED_BYTES || metadata.mode() & 0o6022 != 0 {
        return Err(denied());
    }
    if let Some(expected) = expected {
        let mut hasher = Sha256::new();
        let mut reader = (&file).take(MAX_PINNED_BYTES);
        let mut buffer = vec![0; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buffer[..n]),
                Err(_) => return Err(denied()),
            }
        }
        let actual: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if &actual != expected {
            return Err(denied());
        }
    }
    Ok((path.to_owned(), file, metadata))
}

fn prepare_stdio(
    server: &Server,
    home: &Path,
    runtime_roots: &[PathBuf],
    excluded_roots: &[PathBuf],
    limits: Limits,
) -> Result<Option<(SandboxPolicy, StdioResources)>, polaris_mcp::Error> {
    let Destination::Stdio {
        command,
        cwd,
        sandbox_profile,
        readable_roots,
        executable_sha256,
        script,
        script_sha256,
        ..
    } = &server.transport
    else {
        return Ok(None);
    };
    for root in readable_roots {
        admit_read_root(root, home, cwd, excluded_roots)?;
    }
    let runtime_roots: Vec<PathBuf> = runtime_roots
        .iter()
        .chain(readable_roots)
        .cloned()
        .collect();
    if excluded_roots
        .iter()
        .any(|root| root.starts_with(cwd) || cwd.starts_with(root))
    {
        return Err(polaris_mcp::Error::Protection);
    }
    let directory =
        RegisteredDirectory::open(cwd, limits).map_err(|_| polaris_mcp::Error::Protection)?;
    let mode = match sandbox_profile.as_str() {
        "read-only" | "loopback" => SandboxMode::ReadOnly,
        "workspace-write" => SandboxMode::WorkspaceWrite,
        "full-access" => SandboxMode::FullAccess,
        _ => return Err(polaris_mcp::Error::Protection),
    };
    let mut policy = SandboxPolicy::isolated_registered(mode, cwd, &runtime_roots)
        .map_err(|_| polaris_mcp::Error::Protection)?;
    if sandbox_profile == "loopback" {
        policy = policy
            .with_loopback_network()
            .map_err(|_| polaris_mcp::Error::Protection)?;
    }
    let readable = |path: &Path| {
        policy.isolated_boundary().is_some_and(|boundary| {
            boundary
                .readable_roots
                .iter()
                .any(|root| path.starts_with(root))
        })
    };
    if !readable(command) || script.as_deref().is_some_and(|script| !readable(script)) {
        return Err(polaris_mcp::Error::Protection);
    }
    let executable = pinned_file(command, executable_sha256.as_ref())?;
    if executable.2.nlink() != 1 || executable.2.mode() & 0o111 == 0 {
        return Err(polaris_mcp::Error::Protection);
    }
    let mut pinned = vec![executable];
    if let Some(script) = script {
        pinned.push(pinned_file(script, script_sha256.as_ref())?);
    }
    let scratch = prepare_scratch(
        &std::env::temp_dir(),
        excluded_roots,
        &policy.isolated_boundary().unwrap().readable_roots,
    )?;
    let container = scratch
        .path()
        .canonicalize()
        .map_err(|_| polaris_mcp::Error::Protection)?;
    let home = container.join("home");
    let tmp = container.join("tmp");
    for path in [&home, &tmp] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| polaris_mcp::Error::Protection)?;
    }
    policy = policy
        .with_isolated_private_environment(&container, &home, &tmp)
        .map_err(|_| polaris_mcp::Error::Protection)?;
    let scratch_directories = [container, home, tmp]
        .into_iter()
        .map(|path| {
            let fd = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .map_err(|_| polaris_mcp::Error::Protection)?;
            Ok((path, fd))
        })
        .collect::<Result<Vec<_>, polaris_mcp::Error>>()?;
    let prepared = StdioResources {
        cwd: directory,
        pinned,
        _scratch: scratch,
        scratch_directories,
    };
    prepared.revalidate()?;
    Ok(Some((policy, prepared)))
}

impl RuntimeConfig {
    /// Save-time check with owner startup's stdio rules. The caller's source
    /// and store roots apply only to servers enabled for its project; other
    /// projects' roots are unknown here. Scratch directories are removed.
    pub fn validate_servers<'a>(
        servers: impl IntoIterator<Item = &'a Server>,
        project: &str,
        home: &Path,
        runtime_roots: &[PathBuf],
        excluded_roots: &[PathBuf],
        limits: Limits,
    ) -> Result<(), polaris_mcp::Error> {
        for server in servers {
            let excluded = if server.enabled_projects.contains(project) {
                excluded_roots
            } else {
                &[]
            };
            prepare_stdio(server, home, runtime_roots, excluded, limits)?;
        }
        Ok(())
    }

    /// Consume one locked host snapshot after credential registration and before
    /// model-visible reads. Missing secrets remain absent; no store is invented.
    /// `excluded_roots` are task/source/store roots that stdio must not inherit.
    /// Only explicit trusted runtime roots and host-derived install roots are readable.
    pub fn prepare_registered(
        snapshot: Snapshot,
        project: String,
        home: &Path,
        runtime_roots: &[PathBuf],
        excluded_roots: &[PathBuf],
        limits: Limits,
    ) -> Result<Option<Self>, polaris_mcp::Error> {
        let Some(registry) = snapshot.registry else {
            return Ok(None);
        };
        registry.validate()?;
        if !registry
            .servers
            .iter()
            .any(|s| s.enabled_projects.contains(&project))
        {
            return Ok(None);
        }
        let mut registry = registry;
        let mut policies = BTreeMap::new();
        let mut resources = BTreeMap::new();
        let mut quarantined = BTreeMap::new();
        for server in registry
            .servers
            .iter()
            .filter(|s| s.enabled_projects.contains(&project))
        {
            let references = match &server.transport {
                Destination::Stdio { secret_env, .. } => secret_env,
                Destination::StreamableHttp { secret_headers, .. } => secret_headers,
            };
            let secrets_found = references.values().all(|reference| {
                snapshot
                    .secrets
                    .as_ref()
                    .is_some_and(|secrets| secrets.resolve(reference).is_ok())
            });
            // A failing server is disabled with its reason; the owner still starts.
            match prepare_stdio(server, home, runtime_roots, excluded_roots, limits) {
                _ if !secrets_found => {
                    quarantined.insert(server.server_id.clone(), polaris_mcp::Error::Auth);
                }
                Ok(Some((policy, prepared))) => {
                    policies.insert(server.server_id.clone(), policy);
                    resources.insert(server.server_id.clone(), Arc::new(prepared));
                }
                Ok(None) => {}
                Err(error) => {
                    quarantined.insert(server.server_id.clone(), error);
                }
            }
        }
        registry
            .servers
            .retain(|server| !quarantined.contains_key(&server.server_id));
        let mut config = Self::new(registry, project, snapshot.secrets.map(Arc::new), policies)?;
        config.stdio_resources = resources;
        config.quarantined = quarantined;
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests;
