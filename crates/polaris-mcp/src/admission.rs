//! Shared read-only stdio resource admission for startup and store migration.
//! No child, connection, scratch directory or store write occurs here.
use crate::store::migration_v2::mapped::MappedStagingViewV2;
use crate::{
    Error,
    registration::{Destination, Server},
};
use polaris_sandbox::{SandboxMode, SandboxPolicy, policy::RegisteredBoundaryLayout};
use polaris_tools::path_policy::tree_admission::{Limits, RegisteredDirectory};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
/// Read-only admission witnesses. The owner retains and revalidates these
/// before using the registered paths; descendants are not frozen.
pub struct StdioAdmission {
    cwd: RegisteredDirectory,
    // Pinned files: the executable and, for interpreted servers, its script.
    pinned: Vec<(PathBuf, File, Metadata)>,
}

impl StdioAdmission {
    pub fn pinned_file_count(&self) -> usize {
        self.pinned.len()
    }

    pub fn revalidate(&self) -> Result<(), Error> {
        self.revalidate_from(ResourceSource::Live)
    }

    fn revalidate_from(&self, source: ResourceSource<'_, '_>) -> Result<(), Error> {
        for pin in &self.pinned {
            source.revalidate_pin(pin)?;
        }
        self.cwd.revalidate().map_err(|_| Error::Protection)?;
        source.close()
    }
}

pub(crate) const MAX_ROOT_ENTRIES: usize = 65_536;
pub(crate) const MAX_ROOT_DEPTH: usize = 64;
const MAX_PINNED_BYTES: u64 = 512 * 1024 * 1024;
const BROAD_ROOTS: &[&str] = &["/usr", "/usr/local", "/opt", "/opt/homebrew", "/opt/local"];

/// An install tree exposed read-only to one server. It is never home, a home
/// ancestor or direct child, a shared prefix, or overlapping the cwd, source or
/// store. Every entry is owned by root or this user and not group/other-writable,
/// links stay inside, and no protected path, identity or denied name is beneath.
pub fn admit_read_root(
    root: &Path,
    home: &Path,
    cwd: &Path,
    excluded_roots: &[PathBuf],
) -> Result<(), Error> {
    let denied = || Error::Protection;
    if root.canonicalize().map_err(|_| denied())? != root {
        return Err(denied());
    }
    validate_read_root_location(root, home, cwd, excluded_roots)?;
    // Copy the protection snapshot; never walk while holding the auth lock.
    let (paths, identities) = polaris_auth::protection::with_protected_paths(|registered| {
        (registered.paths().clone(), registered.identities().clone())
    })
    .map_err(|_| denied())?;
    if paths.iter().any(|path| path.starts_with(root)) {
        return Err(denied());
    }
    let mut stack = vec![(root.to_path_buf(), 0)];
    let mut seen = 0;
    while let Some((path, depth)) = stack.pop() {
        seen += 1;
        if depth > MAX_ROOT_DEPTH {
            return Err(Error::Limit);
        }
        let metadata = path.symlink_metadata().map_err(|_| denied())?;
        validate_read_root_node(&path, &metadata, &identities)?;
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
                validate_read_root_count(seen + stack.len() + 1)?;
                stack.push((entry.map_err(|_| denied())?.path(), depth + 1));
            }
        }
    }
    Ok(())
}

/// Count includes the root and the candidate child. Live checks before push;
/// mapped inventory checks after increment, using this same inclusive limit.
pub(crate) fn validate_read_root_count(entries: usize) -> Result<(), Error> {
    if entries > MAX_ROOT_ENTRIES {
        return Err(Error::Limit);
    }
    Ok(())
}

/// Shared protection/name/owner rule; mode is checked after the live-only
/// in-tree symlink case, which staged inventories reject before admission.
pub(crate) fn validate_read_root_node(
    path: &Path,
    metadata: &Metadata,
    identities: &std::collections::BTreeSet<polaris_auth::protection::ProtectedIdentity>,
) -> Result<(), Error> {
    if polaris_tools::path_policy::is_denied(path)
        || identities.contains(&polaris_auth::protection::ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
        || (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
    {
        return Err(Error::Protection);
    }
    Ok(())
}

/// The logical topology rules are identical for live and staged read roots.
pub(crate) fn validate_read_root_location(
    root: &Path,
    home: &Path,
    cwd: &Path,
    excluded_roots: &[PathBuf],
) -> Result<(), Error> {
    if root.parent().is_none()
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
        return Err(Error::Protection);
    }
    Ok(())
}

/// Closed resource provenance. A staged miss never attempts the live path.
#[derive(Clone, Copy)]
enum ResourceSource<'view, 'guard> {
    Live,
    Mapped(&'view MappedStagingViewV2<'guard>),
}

impl ResourceSource<'_, '_> {
    fn close(self) -> Result<(), Error> {
        match self {
            Self::Live => Ok(()),
            Self::Mapped(view) => view.revalidate(),
        }
    }
    fn directory(self, path: &Path, limits: Limits) -> Result<RegisteredDirectory, Error> {
        match self {
            Self::Mapped(view) if view.contains(path) => view.registered_directory(path, limits),
            _ => RegisteredDirectory::open(path, limits).map_err(|_| Error::Protection),
        }
    }
    fn read_root(
        self,
        path: &Path,
        home: &Path,
        cwd: &Path,
        excluded: &[PathBuf],
    ) -> Result<(), Error> {
        match self {
            Self::Mapped(view) if view.contains(path) => {
                view.admit_read_root(path, home, cwd, excluded)
            }
            _ => admit_read_root(path, home, cwd, excluded),
        }
    }
    fn pin(self, path: &Path, digest: Option<&String>) -> Result<(PathBuf, File, Metadata), Error> {
        match self {
            Self::Mapped(view) if view.contains(path) => view.pinned_file(path, digest),
            _ => pinned_file(path, digest),
        }
    }
    fn revalidate_pin(self, pin: &(PathBuf, File, Metadata)) -> Result<(), Error> {
        if let Self::Mapped(view) = self
            && view.contains(&pin.0)
        {
            return view.revalidate_pin(pin);
        }
        if pin.0.canonicalize().map_err(|_| Error::Protection)? != pin.0
            || !same_metadata(
                &pin.0.symlink_metadata().map_err(|_| Error::Protection)?,
                &pin.2,
            )
            || !same_metadata(&pin.1.metadata().map_err(|_| Error::Protection)?, &pin.2)
        {
            return Err(Error::Protection);
        }
        Ok(())
    }
    fn boundary(
        self,
        mode: SandboxMode,
        cwd: &Path,
        roots: &[PathBuf],
        loopback: bool,
    ) -> Result<Boundary, Error> {
        match self {
            Self::Live => {
                let mut policy = SandboxPolicy::isolated_registered(mode, cwd, roots)
                    .map_err(|_| Error::Protection)?;
                if loopback {
                    policy = policy
                        .with_loopback_network()
                        .map_err(|_| Error::Protection)?;
                }
                Ok(Boundary::Live(policy))
            }
            Self::Mapped(view) => {
                let normalize = |path: &Path| -> Result<PathBuf, Error> {
                    if view.contains(path) {
                        view.open_readonly(path)?;
                        Ok(path.to_owned())
                    } else {
                        path.canonicalize().map_err(|_| Error::Protection)
                    }
                };
                let cwd = normalize(cwd)?;
                let roots = roots
                    .iter()
                    .map(|root| normalize(root))
                    .collect::<Result<Vec<_>, _>>()?;
                let layout = RegisteredBoundaryLayout::new(mode, &cwd, &roots)
                    .map_err(|_| Error::Protection)?;
                view.revalidate()?;
                Ok(Boundary::Mapped(layout))
            }
        }
    }
}

// Only the live branch can return an execution policy. Staged layout never
// acquires a conversion or public constructor for executable admission.
enum Boundary {
    Live(SandboxPolicy),
    Mapped(RegisteredBoundaryLayout),
}
impl Boundary {
    fn readable(&self, path: &Path) -> bool {
        match self {
            Self::Live(policy) => policy.isolated_boundary().is_some_and(|boundary| {
                boundary
                    .readable_roots
                    .iter()
                    .any(|root| path.starts_with(root))
            }),
            Self::Mapped(layout) => layout.contains_readable(path),
        }
    }
}

/// Open without following links, require a bounded regular file, and compare
/// the digest read from that same descriptor.
fn pinned_file(path: &Path, expected: Option<&String>) -> Result<(PathBuf, File, Metadata), Error> {
    let denied = || Error::Protection;
    if path.canonicalize().map_err(|_| denied())? != path {
        return Err(denied());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| denied())?;
    inspect_pinned_file(path, file, expected)
}

/// Validate the exact descriptor supplied by a closed resource owner. This
/// performs no path resolution and grants no execution authority. The owner
/// must close its namespace/inventory checks after this read.
pub(crate) fn inspect_pinned_file(
    path: &Path,
    file: File,
    expected: Option<&String>,
) -> Result<(PathBuf, File, Metadata), Error> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let denied = || Error::Protection;
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
    if !same_metadata(&metadata, &file.metadata().map_err(|_| denied())?) {
        return Err(denied());
    }
    Ok((path.to_owned(), file, metadata))
}

/// The executable rule is shared by live and inventoried staging admission.
pub(crate) fn require_executable(metadata: &Metadata) -> Result<(), Error> {
    if metadata.nlink() != 1 || metadata.mode() & 0o111 == 0 {
        return Err(Error::Protection);
    }
    Ok(())
}

pub fn same_metadata(a: &Metadata, b: &Metadata) -> bool {
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
}

pub fn admit_stdio(
    server: &Server,
    home: &Path,
    runtime_roots: &[PathBuf],
    excluded_roots: &[PathBuf],
    limits: Limits,
) -> Result<Option<(SandboxPolicy, StdioAdmission)>, Error> {
    match admit_stdio_from(
        server,
        home,
        runtime_roots,
        excluded_roots,
        limits,
        ResourceSource::Live,
    )? {
        Some((Boundary::Live(policy), witness)) => Ok(Some((policy, witness))),
        None => Ok(None),
        Some((Boundary::Mapped(_), _)) => Err(Error::Protection),
    }
}

fn admit_stdio_from(
    server: &Server,
    home: &Path,
    runtime_roots: &[PathBuf],
    excluded_roots: &[PathBuf],
    limits: Limits,
    source: ResourceSource<'_, '_>,
) -> Result<Option<(Boundary, StdioAdmission)>, Error> {
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
        source.read_root(root, home, cwd, excluded_roots)?;
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
        return Err(Error::Protection);
    }
    let directory = source.directory(cwd, limits)?;
    let mode = match sandbox_profile.as_str() {
        "read-only" | "loopback" => SandboxMode::ReadOnly,
        "workspace-write" => SandboxMode::WorkspaceWrite,
        "full-access" => SandboxMode::FullAccess,
        _ => return Err(Error::Protection),
    };
    let policy = source.boundary(mode, cwd, &runtime_roots, sandbox_profile == "loopback")?;
    if !policy.readable(command)
        || script
            .as_deref()
            .is_some_and(|script| !policy.readable(script))
    {
        return Err(Error::Protection);
    }
    let executable = source.pin(command, executable_sha256.as_ref())?;
    require_executable(&executable.2)?;
    let mut pinned = vec![executable];
    if let Some(script) = script {
        pinned.push(source.pin(script, script_sha256.as_ref())?);
    }
    Ok(Some((
        policy,
        StdioAdmission {
            cwd: directory,
            pinned,
        },
    )))
}

/// Retain every stdio witness until all servers have been admitted, then
/// revalidate the whole set. Exclusions apply to the selected project's servers.
pub fn validate_servers_no_exec<'a>(
    servers: impl IntoIterator<Item = &'a Server>,
    project: &str,
    home: &Path,
    runtime_roots: &[PathBuf],
    excluded_roots: &[PathBuf],
    limits: Limits,
) -> Result<Vec<StdioAdmission>, Error> {
    let mut admissions = Vec::new();
    for server in servers {
        let excluded = if server.enabled_projects.contains(project) {
            excluded_roots
        } else {
            &[]
        };
        if let Some((_, admission)) = admit_stdio(server, home, runtime_roots, excluded, limits)? {
            admissions.push(admission);
        }
    }
    for admission in &admissions {
        admission.revalidate()?;
    }
    Ok(admissions)
}

/// Private read-only registry resource witness. It borrows the real staging
/// view and has no Clone/Serde or execution/publication conversion.
pub struct MappedRegistryAdmissionV2<'view, 'guard> {
    view: &'view MappedStagingViewV2<'guard>,
    admissions: Vec<StdioAdmission>,
    layouts: Vec<RegisteredBoundaryLayout>,
}
impl MappedRegistryAdmissionV2<'_, '_> {
    pub fn revalidate(&self) -> Result<(), Error> {
        for admission in &self.admissions {
            admission.revalidate_from(ResourceSource::Mapped(self.view))?;
        }
        self.view.revalidate()
    }
    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.layouts.len()
    }
}

/// Project selection and its resource exclusions for no-exec admission.
pub struct ProjectAdmissionScope<'a> {
    project: &'a str,
    excluded_roots: &'a [PathBuf],
}
impl<'a> ProjectAdmissionScope<'a> {
    pub fn new(project: &'a str, excluded_roots: &'a [PathBuf]) -> Self {
        Self {
            project,
            excluded_roots,
        }
    }
}

/// Complete source-to-staging document binding remains with the coordinator.
/// Schema and reference checks precede admission of every registry server's resource
/// set, retains all FDs, then closes the whole set and the mapped inventory.
pub fn validate_mapped_servers_no_exec<'view, 'guard>(
    view: &'view MappedStagingViewV2<'guard>,
    registry: &crate::registration::Registry,
    credential: &crate::store::documents_v2::CredentialDocumentV2,
    scope: ProjectAdmissionScope<'_>,
    home: &Path,
    runtime_roots: &[PathBuf],
    limits: Limits,
) -> Result<MappedRegistryAdmissionV2<'view, 'guard>, Error> {
    registry.validate()?;
    credential.validate_registry_references(registry)?;
    view.revalidate()?;
    let mut value = MappedRegistryAdmissionV2 {
        view,
        admissions: Vec::new(),
        layouts: Vec::new(),
    };
    for server in &registry.servers {
        let excluded = if server.enabled_projects.contains(scope.project) {
            scope.excluded_roots
        } else {
            &[]
        };
        if let Some((boundary, admission)) = admit_stdio_from(
            server,
            home,
            runtime_roots,
            excluded,
            limits,
            ResourceSource::Mapped(view),
        )? {
            let Boundary::Mapped(layout) = boundary else {
                return Err(Error::Protection);
            };
            value.layouts.push(layout);
            value.admissions.push(admission);
        }
    }
    #[cfg(test)]
    BEFORE_MAPPED_REGISTRY_CLOSE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    value.revalidate()?;
    Ok(value)
}

#[cfg(test)]
thread_local! {
    static BEFORE_MAPPED_REGISTRY_CLOSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn before_mapped_registry_close(hook: impl FnOnce() + 'static) {
    BEFORE_MAPPED_REGISTRY_CLOSE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_root_node_limit_is_inclusive_for_live_push_and_mapped_increment() {
        // Root + MAX-2 queued nodes, just before adding the final child.
        let seen = 1;
        let queued = MAX_ROOT_ENTRIES - 2;
        validate_read_root_count(seen + queued + 1).unwrap();
        validate_read_root_count(MAX_ROOT_ENTRIES).unwrap();
        assert!(matches!(
            validate_read_root_count(seen + queued + 2),
            Err(Error::Limit)
        ));
        assert!(matches!(
            validate_read_root_count(MAX_ROOT_ENTRIES + 1),
            Err(Error::Limit)
        ));
    }
}
