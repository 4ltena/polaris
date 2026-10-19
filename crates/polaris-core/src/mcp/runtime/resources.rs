//! Trusted startup resources, shared with the factory until native cleanup ends.
use super::*;
use crate::isolated_workspace::Limits;
use polaris_mcp::store::Snapshot;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{
    fs::{self, File, OpenOptions, Permissions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
#[cfg(unix)]
use tempfile::TempDir;

// A per-thread test count detects transient scratch attempts too.
#[cfg(all(test, unix))]
std::thread_local! {
    static SCRATCH_PREPARATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(unix)]
fn prepare_scratch(
    parent: &Path,
    excluded_roots: &[PathBuf],
    readable_roots: &[PathBuf],
) -> Result<TempDir, polaris_mcp::Error> {
    #[cfg(test)]
    SCRATCH_PREPARATIONS.with(|count| count.set(count.get() + 1));
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

#[cfg(unix)]
use polaris_mcp::admission::admit_stdio;
#[cfg(unix)]
pub(crate) use polaris_mcp::admission::{StdioAdmission, admit_read_root};

#[cfg(not(unix))]
pub(crate) fn admit_read_root(
    _candidate: &Path,
    _home: &Path,
    _command: &Path,
    _runtime_roots: &[PathBuf],
) -> Result<(), polaris_mcp::Error> {
    Err(polaris_mcp::Error::Protection)
}

#[cfg(unix)]
pub(super) struct StdioResources {
    admission: StdioAdmission,
    _scratch: TempDir,
    scratch_directories: Vec<(PathBuf, File)>,
}

#[cfg(unix)]
impl StdioResources {
    pub(super) fn revalidate(&self) -> Result<(), polaris_mcp::Error> {
        self.admission.revalidate()?;
        let check = || -> std::io::Result<bool> {
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
        Ok(())
    }
}

#[cfg(unix)]
fn prepare_stdio(
    server: &Server,
    home: &Path,
    runtime_roots: &[PathBuf],
    excluded_roots: &[PathBuf],
    limits: Limits,
) -> Result<Option<(SandboxPolicy, StdioResources)>, polaris_mcp::Error> {
    let Some((mut policy, admission)) =
        admit_stdio(server, home, runtime_roots, excluded_roots, limits)?
    else {
        return Ok(None);
    };
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
        admission,
        _scratch: scratch,
        scratch_directories,
    };
    prepared.revalidate()?;
    Ok(Some((policy, prepared)))
}

impl RuntimeConfig {
    /// Admit ordinary registered paths without executing a child or creating
    /// scratch. Apply source/store exclusions to this project's servers exactly
    /// as save-time validation does, and retain every stdio witness until all
    /// servers have been admitted and revalidated. The caller keeps the returned
    /// witnesses and revalidates them immediately before using those paths.
    #[cfg(all(test, unix))]
    pub(crate) fn validate_servers_no_exec<'a>(
        servers: impl IntoIterator<Item = &'a Server>,
        project: &str,
        home: &Path,
        runtime_roots: &[PathBuf],
        excluded_roots: &[PathBuf],
        limits: Limits,
    ) -> Result<Vec<StdioAdmission>, polaris_mcp::Error> {
        polaris_mcp::admission::validate_servers_no_exec(
            servers,
            project,
            home,
            runtime_roots,
            excluded_roots,
            limits,
        )
    }

    /// Save-time check with owner startup's stdio rules. The caller's source
    /// and store roots apply only to servers enabled for its project; other
    /// projects' roots are unknown here. Scratch directories are removed.
    #[cfg(unix)]
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

    #[cfg(not(unix))]
    pub fn validate_servers<'a>(
        _servers: impl IntoIterator<Item = &'a Server>,
        _project: &str,
        _home: &Path,
        _runtime_roots: &[PathBuf],
        _excluded_roots: &[PathBuf],
        _limits: Limits,
    ) -> Result<(), polaris_mcp::Error> {
        Err(polaris_mcp::Error::Protection)
    }

    /// Consume one locked host snapshot after credential registration and before
    /// model-visible reads. Missing secrets remain absent; no store is invented.
    /// `excluded_roots` are task/source/store roots that stdio must not inherit.
    /// Only explicit trusted runtime roots and host-derived install roots are readable.
    #[cfg(unix)]
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

    #[cfg(not(unix))]
    pub fn prepare_registered(
        _snapshot: Snapshot,
        _project: String,
        _home: &Path,
        _runtime_roots: &[PathBuf],
        _excluded_roots: &[PathBuf],
        _limits: Limits,
    ) -> Result<Option<Self>, polaris_mcp::Error> {
        Err(polaris_mcp::Error::Protection)
    }
}

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;
