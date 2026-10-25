//! Policy and writable roots. Roots are normalized at construction time.

use std::path::{Path, PathBuf};

use crate::SandboxError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    FullAccess,
}

impl SandboxMode {
    /// The policy's name. The audit log and the denial message use the same spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::FullAccess => "full-access",
        }
    }
}

/// A logical layout for resource validation, not an execution policy or a
/// filesystem admission. It cannot be converted publicly into SandboxPolicy.
/// A migration owner must separately pin and revalidate every mapped resource.
#[derive(Debug, PartialEq, Eq)]
pub struct RegisteredBoundaryLayout {
    mode: SandboxMode,
    workspace: PathBuf,
    readable_roots: Vec<PathBuf>,
    writable_roots: Vec<PathBuf>,
}

impl RegisteredBoundaryLayout {
    pub fn new(
        mode: SandboxMode,
        workspace: &Path,
        runtime_roots: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let normalized = |path: &Path| {
            let rebuilt: PathBuf = path.components().collect();
            path.is_absolute()
                && path.parent().is_some()
                && rebuilt.as_os_str() == path.as_os_str()
                && path.components().all(|component| {
                    matches!(
                        component,
                        std::path::Component::Prefix(_)
                            | std::path::Component::RootDir
                            | std::path::Component::Normal(_)
                    )
                })
        };
        if !normalized(workspace) {
            return Err(SandboxError::NotEnforced(
                "invalid isolated workspace".into(),
            ));
        }
        let workspace = workspace.to_owned();
        let mut readable_roots = vec![workspace.clone()];
        for root in runtime_roots {
            if !normalized(root) {
                return Err(SandboxError::NotEnforced(
                    "invalid isolated readable root".into(),
                ));
            }
            if !readable_roots.contains(root) {
                readable_roots.push(root.clone());
            }
        }
        Ok(Self {
            mode,
            writable_roots: if mode == SandboxMode::ReadOnly {
                vec![]
            } else {
                vec![workspace.clone()]
            },
            workspace,
            readable_roots,
        })
    }

    pub fn contains_readable(&self, path: &Path) -> bool {
        self.readable_roots
            .iter()
            .any(|root| path.starts_with(root))
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub fn readable_roots(&self) -> &[PathBuf] {
        &self.readable_roots
    }
    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,
    isolated: Option<IsolatedBoundary>,
    require_process_containment: bool,
}

#[cfg(all(test, unix))]
mod registered_layout_tests {
    use super::*;

    #[test]
    fn logical_layout_creates_no_resource_or_execution_policy() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let workspace = root.join("not-published");
        let runtime = root.join("not-installed");
        let layout = RegisteredBoundaryLayout::new(
            SandboxMode::WorkspaceWrite,
            &workspace,
            &[runtime.clone(), workspace.clone(), runtime.clone()],
        )
        .unwrap();
        assert_eq!(layout.workspace(), workspace);
        assert_eq!(
            layout.readable_roots(),
            &[workspace.clone(), runtime.clone()]
        );
        assert_eq!(layout.writable_roots(), std::slice::from_ref(&workspace));
        assert!(layout.contains_readable(&workspace.join("index.js")));
        assert!(!layout.contains_readable(&root.join("foreign/index.js")));
        assert!(
            SandboxPolicy::isolated_registered(
                SandboxMode::WorkspaceWrite,
                &workspace,
                std::slice::from_ref(&runtime),
            )
            .is_err()
        );
        assert!(!workspace.exists());
        assert!(!runtime.exists());
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn logical_layout_rejects_aliases_and_matches_live_policy_topology() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let runtime = root.join("runtime");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&runtime).unwrap();
        for mode in [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::FullAccess,
        ] {
            let layout =
                RegisteredBoundaryLayout::new(mode, &workspace, std::slice::from_ref(&runtime))
                    .unwrap();
            let live = SandboxPolicy::isolated_registered(
                mode,
                &workspace,
                std::slice::from_ref(&runtime),
            )
            .unwrap();
            let boundary = live.isolated_boundary().unwrap();
            assert_eq!(layout.workspace(), boundary.workspace);
            assert_eq!(layout.readable_roots(), boundary.readable_roots);
            assert_eq!(layout.writable_roots(), live.writable_roots());
        }
        for invalid in [
            PathBuf::from("/"),
            PathBuf::from("relative"),
            workspace.join(".."),
            PathBuf::from(format!("{}//workspace", root.display())),
        ] {
            assert!(RegisteredBoundaryLayout::new(SandboxMode::ReadOnly, &invalid, &[]).is_err());
            assert!(
                RegisteredBoundaryLayout::new(SandboxMode::ReadOnly, &workspace, &[invalid])
                    .is_err()
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedBoundary {
    pub workspace: PathBuf,
    pub readable_roots: Vec<PathBuf>,
    /// Exact read-only leaves; these never grant their parent or a subtree.
    pub readable_files: Vec<PathBuf>,
    pub environment: Option<IsolatedEnvironment>,
    /// Connections to and listeners on loopback only; other hosts stay denied.
    pub loopback_network: bool,
}

/// Parent-prepared scratch directories, separate from source write authority.
/// The backend must explicitly allow writes here even for a read-only child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedEnvironment {
    pub home: PathBuf,
    pub tmpdir: PathBuf,
    private_siblings: bool,
    private_container: Option<PathBuf>,
    // Trusted, copied runtime directories only; never inherited host PATH text.
    pub(crate) runtime_bin_dirs: Vec<PathBuf>,
    pub(crate) fixed_values: std::collections::BTreeMap<String, String>,
}

impl IsolatedEnvironment {
    pub(crate) fn validate(&self, workspace: &Path) -> Result<(), SandboxError> {
        validate_fixed_values(&self.fixed_values)?;
        #[cfg(not(unix))]
        if self.private_container.is_some() {
            return Err(SandboxError::NotEnforced(
                "private scratch is unsupported".into(),
            ));
        }
        #[cfg(unix)]
        if let Some(container) = &self.private_container {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                for path in [container, &self.home, &self.tmpdir] {
                    let metadata = path.symlink_metadata()?;
                    if !metadata.is_dir()
                        || metadata.uid() != unsafe { libc::geteuid() }
                        || metadata.mode() & 0o7077 != 0
                        || path.canonicalize()? != *path
                    {
                        return Err(SandboxError::NotEnforced(
                            "invalid private scratch directory".into(),
                        ));
                    }
                }
            }
            if container.starts_with(workspace)
                || workspace.starts_with(container)
                || self.home.parent() != Some(container)
                || self.tmpdir.parent() != Some(container)
            {
                return Err(SandboxError::NotEnforced(
                    "private scratch overlaps workspace".into(),
                ));
            }
        }
        for path in [&self.home, &self.tmpdir] {
            if path.canonicalize()? != *path
                || !path.is_dir()
                || path == workspace
                || !(path.starts_with(workspace)
                    || (self.private_siblings && path.parent() == workspace.parent())
                    || self.private_container.is_some())
            {
                return Err(SandboxError::NotEnforced(
                    "isolated environment must be a dedicated workspace child or private-root sibling".into(),
                ));
            }
        }
        if self.home.starts_with(&self.tmpdir) || self.tmpdir.starts_with(&self.home) {
            return Err(SandboxError::NotEnforced(
                "isolated home and tmpdir must not overlap".into(),
            ));
        }
        for bin in &self.runtime_bin_dirs {
            if bin.canonicalize()? != *bin || !bin.is_dir() {
                return Err(SandboxError::NotEnforced(
                    "runtime bin directory changed".into(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_fixed_values(
    values: &std::collections::BTreeMap<String, String>,
) -> Result<(), SandboxError> {
    if values.len() > 32 {
        return Err(SandboxError::NotEnforced(
            "too many fixed environment values".into(),
        ));
    }
    for (name, value) in values {
        let allowed = match name.as_str() {
            "CI"
            | "NEXT_TELEMETRY_DISABLED"
            | "NUXT_TELEMETRY_DISABLED"
            | "ASTRO_TELEMETRY_DISABLED"
            | "PYTHONNOUSERSITE"
            | "PIP_NO_INDEX"
            | "PIP_DISABLE_PIP_VERSION_CHECK" => value == "1",
            "PIP_CONFIG_FILE" => value == "/dev/null",
            "CARGO_NET_OFFLINE" => value == "true",
            "GOTOOLCHAIN" => value == "local",
            "GOPROXY" | "GOSUMDB" | "GOENV" | "GOWORK" => value == "off",
            "CGO_ENABLED" => value == "0",
            "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER" => value.is_empty(),
            "CARGO_HOME"
            | "GOPATH"
            | "GOCACHE"
            | "NEXT_PRIVATE_OUTPUT_TRACE_ROOT"
            | "DEVELOPER_DIR"
            | "SDKROOT"
            | "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER" => {
                Path::new(value).is_absolute() && !value.chars().any(char::is_control)
            }
            // Values come from a revalidated compiled recipe, not from model,
            // project config or inherited environment. US is Cargo's delimiter.
            "CARGO_ENCODED_RUSTFLAGS" => {
                !value.is_empty() && !value.chars().any(|c| c.is_control() && c != '\u{1f}')
            }
            _ => false,
        };
        if !allowed || value.len() > 16 * 1024 || value.as_bytes().contains(&0) {
            return Err(SandboxError::NotEnforced(
                "invalid fixed environment value".into(),
            ));
        }
    }
    Ok(())
}

const MAX_ISOLATED_READABLE_FILES: usize = 16;

fn is_fixed_entry_gate_file(path: &Path) -> bool {
    path.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".polaris"))
        && matches!(
            path.file_name().and_then(std::ffi::OsStr::to_str),
            Some(
                "mcp-entry-gate-v2.json"
                    | "mcp-entry-gate-v2.lock"
                    | "mcp-entry-identities-v2.json"
            )
        )
}

/// Validate spelling and every existing component without resolving an alias.
/// Only the fixed entry-gate leaf and its .polaris parent may be absent.
/// The entry owner must pin and revalidate identities immediately before launch.
fn validate_isolated_readable_file(path: &Path) -> Result<(), SandboxError> {
    use std::path::Component;

    let text = path
        .to_str()
        .ok_or_else(|| SandboxError::NotEnforced("readable file path must be UTF-8".into()))?;
    if !path.is_absolute()
        || text.len() > 4096
        || text.chars().any(char::is_control)
        || path.parent().is_none()
    {
        return Err(SandboxError::NotEnforced(
            "unsafe readable file path".into(),
        ));
    }
    let mut spelling = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                spelling.push(component.as_os_str());
            }
            Component::CurDir | Component::ParentDir => {
                return Err(SandboxError::NotEnforced(
                    "readable file path is not normalized".into(),
                ));
            }
        }
    }
    if spelling.as_os_str() != path.as_os_str() {
        return Err(SandboxError::NotEnforced(
            "readable file path is not normalized".into(),
        ));
    }

    let fixed_gate = is_fixed_entry_gate_file(path);
    let mut physical = PathBuf::new();
    let mut absent = false;
    for component in path.components() {
        physical.push(component.as_os_str());
        if absent {
            continue;
        }
        match physical.symlink_metadata() {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || (physical != path && !metadata.is_dir())
                    || (physical == path && !metadata.is_file())
                    || physical.canonicalize()?.as_os_str() != physical.as_os_str()
                {
                    return Err(SandboxError::NotEnforced(
                        "readable file is not a physical regular file".into(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !fixed_gate || (physical != path && Some(physical.as_path()) != path.parent()) {
                    return Err(SandboxError::NotEnforced("readable file is missing".into()));
                }
                absent = true;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_isolated_readable_files(
    files: &[PathBuf],
    writable_roots: &[PathBuf],
    environment: Option<&IsolatedEnvironment>,
) -> Result<(), SandboxError> {
    if files.len() > MAX_ISOLATED_READABLE_FILES {
        return Err(SandboxError::NotEnforced("too many readable files".into()));
    }
    let mut unique = std::collections::BTreeSet::new();
    for file in files {
        validate_isolated_readable_file(file)?;
        if !unique.insert(file) {
            return Err(SandboxError::NotEnforced("duplicate readable file".into()));
        }
        let overlaps = |root: &Path| file.starts_with(root) || root.starts_with(file);
        if writable_roots.iter().any(|root| overlaps(root))
            || environment.is_some_and(|env| {
                overlaps(&env.home)
                    || overlaps(&env.tmpdir)
                    || env
                        .private_container
                        .as_ref()
                        .is_some_and(|root| overlaps(root))
            })
        {
            return Err(SandboxError::NotEnforced(
                "readable file overlaps writable or private scratch area".into(),
            ));
        }
    }
    Ok(())
}

impl SandboxPolicy {
    /// Roots are always normalized before being held. If a symlink appears
    /// partway through the path, the enforcement side judges the given path
    /// and the actual path to be different things and denies everything.
    /// macOS's `/tmp` and `/var` are exactly this case, so it isn't an edge
    /// case.
    pub fn new(mode: SandboxMode, roots: &[PathBuf]) -> Result<Self, SandboxError> {
        match mode {
            SandboxMode::ReadOnly if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "a writable root cannot be specified for read-only".into(),
                ));
            }
            SandboxMode::WorkspaceWrite if roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "workspace-write needs at least one writable root".into(),
                ));
            }
            SandboxMode::FullAccess if !roots.is_empty() => {
                return Err(SandboxError::NotEnforced(
                    "a writable root cannot be specified for full-access".into(),
                ));
            }
            _ => {}
        }

        // full-access doesn't hold onto roots. Holding them would let a value
        // that has no actual effect show up in the policy's description and
        // mislead the reader.
        let writable_roots = if mode == SandboxMode::FullAccess {
            Vec::new()
        } else {
            let mut canonical = Vec::with_capacity(roots.len());
            for r in roots {
                canonical.push(r.canonicalize()?);
            }
            canonical
        };

        Ok(Self {
            mode,
            writable_roots,
            isolated: None,
            require_process_containment: false,
        })
    }

    /// The caller must supply a sanitized, privately owned copy and trusted runtimes.
    /// This constructor does not sanitize a source tree or grant access to its original.
    pub fn isolated(
        mode: SandboxMode,
        workspace: &Path,
        runtime_roots: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        Self::isolated_boundary_policy(mode, workspace, runtime_roots)
    }

    /// The trusted host must admit and pin a dedicated registered cwd, reject
    /// secrets/aliases/unsafe entries, and revalidate before each spawn. This
    /// does not sanitize it or authorize writes outside the registered subtree.
    pub fn isolated_registered(
        mode: SandboxMode,
        workspace: &Path,
        runtime_roots: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        Self::isolated_boundary_policy(mode, workspace, runtime_roots)
    }

    fn isolated_boundary_policy(
        mode: SandboxMode,
        workspace: &Path,
        runtime_roots: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let workspace = workspace.canonicalize()?;
        let mut canonical_roots = Vec::new();
        for root in runtime_roots {
            let root = root.canonicalize()?;
            if root.parent().is_none() {
                return Err(SandboxError::NotEnforced(
                    "cannot expose filesystem root".into(),
                ));
            }
            canonical_roots.push(root);
        }
        if workspace.parent().is_none() || !workspace.is_dir() {
            return Err(SandboxError::NotEnforced(
                "invalid isolated workspace".into(),
            ));
        }
        let layout = RegisteredBoundaryLayout::new(mode, &workspace, &canonical_roots)?;
        Ok(Self {
            mode: layout.mode,
            writable_roots: layout.writable_roots,
            isolated: Some(IsolatedBoundary {
                workspace: layout.workspace,
                readable_roots: layout.readable_roots,
                readable_files: Vec::new(),
                environment: None,
                loopback_network: false,
            }),
            require_process_containment: false,
        })
    }

    pub fn isolated_boundary(&self) -> Option<&IsolatedBoundary> {
        self.isolated.as_ref()
    }

    /// Register at most sixteen physical file paths for exact read-only access.
    /// The host supplies verified entry-manifest files and fixed gate paths,
    /// then revalidates identities before spawn. Only the three fixed gate
    /// paths may be absent; parent contents and subtrees stay ungranted.
    pub fn with_isolated_readable_files(mut self, files: &[PathBuf]) -> Result<Self, SandboxError> {
        let boundary = self.isolated.as_mut().ok_or_else(|| {
            SandboxError::NotEnforced("readable files require an isolated policy".into())
        })?;
        validate_isolated_readable_files(
            files,
            &self.writable_roots,
            boundary.environment.as_ref(),
        )?;
        boundary.readable_files = files.to_vec();
        Ok(self)
    }

    /// Common-case execution opts into a no-fork profile. This is required to
    /// prove that a passed case cannot retain a setsid descendant outside the
    /// direct child's process group.
    pub fn require_process_containment(mut self) -> Result<Self, SandboxError> {
        if self.isolated.is_none() {
            return Err(SandboxError::NotEnforced(
                "process containment requires an isolated policy".into(),
            ));
        }
        self.require_process_containment = true;
        Ok(self)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn requires_process_containment(&self) -> bool {
        self.require_process_containment
    }

    /// A read-only isolated child that talks to a local app over loopback.
    pub fn with_loopback_network(mut self) -> Result<Self, SandboxError> {
        let read_only = self.mode == SandboxMode::ReadOnly;
        match self.isolated.as_mut() {
            Some(boundary) if read_only => boundary.loopback_network = true,
            _ => {
                return Err(SandboxError::NotEnforced(
                    "loopback network requires a read-only isolated policy".into(),
                ));
            }
        }
        Ok(self)
    }

    /// Register existing, dedicated directories prepared by the snapshot owner.
    /// This does not create directories or grant source writes. Siblings must
    /// belong to the same privately owned snapshot container. Child scratch
    /// directories must be excluded from source-copy conflict/apply logic.
    pub fn with_isolated_environment(
        mut self,
        home: &Path,
        tmpdir: &Path,
    ) -> Result<Self, SandboxError> {
        let boundary = self.isolated.as_mut().ok_or_else(|| {
            SandboxError::NotEnforced("isolated environment requires an isolated policy".into())
        })?;
        let environment = IsolatedEnvironment {
            home: home.canonicalize()?,
            tmpdir: tmpdir.canonicalize()?,
            private_siblings: false,
            private_container: None,
            runtime_bin_dirs: Vec::new(),
            fixed_values: std::collections::BTreeMap::new(),
        };
        environment.validate(&boundary.workspace)?;
        validate_isolated_readable_files(
            &boundary.readable_files,
            &self.writable_roots,
            Some(&environment),
        )?;
        boundary.environment = Some(environment);
        Ok(self)
    }

    /// The caller owns the common private container for workspace/home/tmpdir.
    /// Unlike source children, these scratch siblings are never source changes.
    pub fn with_isolated_sibling_environment(
        mut self,
        home: &Path,
        tmpdir: &Path,
    ) -> Result<Self, SandboxError> {
        let boundary = self.isolated.as_mut().ok_or_else(|| {
            SandboxError::NotEnforced("isolated environment requires an isolated policy".into())
        })?;
        let environment = IsolatedEnvironment {
            home: home.canonicalize()?,
            tmpdir: tmpdir.canonicalize()?,
            private_siblings: true,
            private_container: None,
            runtime_bin_dirs: Vec::new(),
            fixed_values: std::collections::BTreeMap::new(),
        };
        if environment.home.parent() != boundary.workspace.parent()
            || environment.tmpdir.parent() != boundary.workspace.parent()
        {
            return Err(SandboxError::NotEnforced(
                "scratch directories must share the private container".into(),
            ));
        }
        environment.validate(&boundary.workspace)?;
        validate_isolated_readable_files(
            &boundary.readable_files,
            &self.writable_roots,
            Some(&environment),
        )?;
        boundary.environment = Some(environment);
        Ok(self)
    }

    /// The host owns a separate private container for both scratch directories
    /// and retains it until child/group cleanup completes. No container-wide
    /// access is granted; only home and tmpdir become readable/writable.
    pub fn with_isolated_private_environment(
        mut self,
        container: &Path,
        home: &Path,
        tmpdir: &Path,
    ) -> Result<Self, SandboxError> {
        let boundary = self.isolated.as_mut().ok_or_else(|| {
            SandboxError::NotEnforced("private environment requires an isolated policy".into())
        })?;
        let environment = IsolatedEnvironment {
            home: home.into(),
            tmpdir: tmpdir.into(),
            private_siblings: false,
            private_container: Some(container.into()),
            runtime_bin_dirs: Vec::new(),
            fixed_values: std::collections::BTreeMap::new(),
        };
        environment.validate(&boundary.workspace)?;
        validate_isolated_readable_files(
            &boundary.readable_files,
            &self.writable_roots,
            Some(&environment),
        )?;
        if boundary
            .readable_roots
            .iter()
            .any(|root| root.starts_with(container) || container.starts_with(root))
        {
            return Err(SandboxError::NotEnforced(
                "private scratch overlaps readable roots".into(),
            ));
        }
        boundary.environment = Some(environment);
        Ok(self)
    }

    /// Add only trusted runtime bin directories to the fixed isolated PATH.
    /// The owner must first copy/validate the runtime; this does not copy it.
    /// Roots must already be readable and disjoint from every writable area.
    pub fn with_isolated_runtime_bins(mut self, bins: &[PathBuf]) -> Result<Self, SandboxError> {
        let boundary = self.isolated.as_mut().ok_or_else(|| {
            SandboxError::NotEnforced("runtime bins require an isolated policy".into())
        })?;
        let environment = boundary.environment.as_mut().ok_or_else(|| {
            SandboxError::NotEnforced("runtime bins require an isolated environment".into())
        })?;
        if bins.len() > 32 {
            return Err(SandboxError::NotEnforced(
                "too many runtime bin directories".into(),
            ));
        }
        for bin in bins {
            if !bin.is_absolute()
                || bin.as_os_str().len() > 4096
                || bin.canonicalize()? != *bin
                || !bin.is_dir()
                || std::env::join_paths([bin]).is_err()
                || !boundary
                    .readable_roots
                    .iter()
                    .any(|root| bin.starts_with(root))
                || [&boundary.workspace, &environment.home, &environment.tmpdir]
                    .into_iter()
                    .chain(self.writable_roots.iter())
                    .any(|root| bin.starts_with(root) || root.starts_with(bin))
            {
                return Err(SandboxError::NotEnforced(
                    "unsafe runtime bin directory".into(),
                ));
            }
        }
        environment.runtime_bin_dirs = bins.to_vec();
        Ok(self)
    }

    /// Trusted-owner only: apply a finite, recipe-bound environment after
    /// env_clear. This cannot set PATH, HOME, proxies, auth or loader overrides.
    pub fn with_isolated_fixed_values(
        mut self,
        values: std::collections::BTreeMap<String, String>,
    ) -> Result<Self, SandboxError> {
        validate_fixed_values(&values)?;
        let environment = self
            .isolated
            .as_mut()
            .and_then(|boundary| boundary.environment.as_mut())
            .ok_or_else(|| {
                SandboxError::NotEnforced("fixed values require an isolated environment".into())
            })?;
        environment.fixed_values = values;
        Ok(self)
    }

    /// Derive a child without widening the parent's authority. Isolated children
    /// retain the readable boundary and workspace (the execution cwd); `roots`
    /// only narrows writes. Even isolated FullAccess requires explicit roots.
    /// Legacy constructor semantics remain unchanged for non-isolated children.
    pub fn restrict(&self, mode: SandboxMode, roots: &[PathBuf]) -> Result<Self, SandboxError> {
        if mode == SandboxMode::FullAccess && self.mode != SandboxMode::FullAccess {
            return Err(SandboxError::NotEnforced(
                "child cannot elevate to full-access".into(),
            ));
        }
        if let Some(boundary) = &self.isolated {
            // Do not silently rebind a saved boundary after a root disappears or
            // is replaced by a symlink. This is a derivation-time check, not an
            // inode identity guarantee against concurrent external replacement.
            for root in boundary
                .readable_roots
                .iter()
                .chain(self.writable_roots.iter())
            {
                if root.canonicalize()? != *root {
                    return Err(SandboxError::NotEnforced("inherited root changed".into()));
                }
            }
            if !boundary.workspace.is_dir() {
                return Err(SandboxError::NotEnforced(
                    "isolated workspace is missing".into(),
                ));
            }
            if let Some(environment) = &boundary.environment {
                environment.validate(&boundary.workspace)?;
            }
            validate_isolated_readable_files(
                &boundary.readable_files,
                &self.writable_roots,
                boundary.environment.as_ref(),
            )?;
        }
        let construction_mode = if self.isolated.is_some() && mode == SandboxMode::FullAccess {
            SandboxMode::WorkspaceWrite
        } else {
            mode
        };
        let mut child = Self::new(construction_mode, roots)?;
        for root in &child.writable_roots {
            if !self.contains(root) {
                return Err(SandboxError::NotEnforced(
                    "child write root is outside the parent's own writable roots".into(),
                ));
            }
        }
        child.mode = mode;
        child.isolated = self.isolated.clone();
        Ok(child)
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// For denial messages. The spec requires including the denied path, the
    /// current policy, and the writable roots. This covers the latter two.
    pub fn describe(&self) -> String {
        if self.writable_roots.is_empty() {
            return self.mode.as_str().to_string();
        }
        let roots: Vec<String> = self
            .writable_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!("{} (writable: {})", self.mode.as_str(), roots.join(", "))
    }

    /// Whether the normalized target path is inside any of the roots.
    /// Legacy `full-access` is always true; isolated modes honor writable roots.
    /// `read-only` is always false.
    pub fn contains(&self, canonical_target: &Path) -> bool {
        if self.isolated.is_some() {
            return self.mode != SandboxMode::ReadOnly
                && self
                    .writable_roots
                    .iter()
                    .any(|root| canonical_target.starts_with(root));
        }
        match self.mode {
            SandboxMode::FullAccess => true,
            SandboxMode::ReadOnly => false,
            SandboxMode::WorkspaceWrite => self
                .writable_roots
                .iter()
                .any(|r| canonical_target.starts_with(r)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn windows_sandbox_unavailable_private_environment_precedes_path_validation() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let home = workspace.join("home");
        let tmpdir = workspace.join("tmp");
        let runtime = workspace.join("runtime");
        for directory in [&home, &tmpdir, &runtime] {
            std::fs::create_dir(directory).unwrap();
        }
        let retained = workspace.join("retained.bin");
        let retained_bytes = b"retained\0fixture\xff";
        std::fs::write(&retained, retained_bytes).unwrap();
        fn contents(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
            fn visit(
                root: &Path,
                path: &Path,
                files: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
            ) {
                for entry in std::fs::read_dir(path).unwrap() {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    let kind = entry.file_type().unwrap();
                    assert!(!kind.is_symlink());
                    let relative = path.strip_prefix(root).unwrap().to_owned();
                    if kind.is_dir() {
                        files.insert(relative, None);
                        visit(root, &path, files);
                    } else {
                        files.insert(relative, Some(std::fs::read(path).unwrap()));
                    }
                }
            }
            let mut files = std::collections::BTreeMap::new();
            visit(root, root, &mut files);
            files
        }
        let before_contents = contents(&workspace);
        let ordinary = IsolatedEnvironment {
            home,
            tmpdir,
            private_siblings: false,
            private_container: None,
            runtime_bin_dirs: vec![runtime],
            fixed_values: std::collections::BTreeMap::new(),
        };
        assert!(ordinary.validate(&workspace).is_ok());
        for container in [
            workspace.clone(),
            workspace.join("missing-container"),
            PathBuf::from("relative-container"),
        ] {
            let mut private = ordinary.clone();
            private.private_container = Some(container);
            private.home = workspace.join("missing-home");
            private.tmpdir = workspace.join("missing-tmp");
            private.runtime_bin_dirs = vec![workspace.join("missing-runtime")];
            assert!(matches!(
                private.validate(&workspace),
                Err(SandboxError::NotEnforced(reason))
                    if reason == "private scratch is unsupported"
            ));
            assert_eq!(contents(&workspace), before_contents);
            assert_eq!(std::fs::read(&retained).unwrap(), retained_bytes);
        }
        assert!(ordinary.validate(&workspace).is_ok());
        assert_eq!(contents(&workspace), before_contents);
    }

    #[test]
    fn exact_readable_files_are_bounded_and_preserved_by_read_only_restrict() {
        let workspace = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let file_root = files.path().canonicalize().unwrap();
        let leaves: Vec<PathBuf> = (0..16)
            .map(|i| file_root.join(format!("file-{i}")))
            .collect();
        for file in &leaves {
            std::fs::write(file, b"nonsecret").unwrap();
        }
        let parent = SandboxPolicy::isolated(SandboxMode::WorkspaceWrite, workspace.path(), &[])
            .unwrap()
            .with_isolated_readable_files(&leaves)
            .unwrap();
        let child = parent.restrict(SandboxMode::ReadOnly, &[]).unwrap();
        assert_eq!(child.isolated_boundary().unwrap().readable_files, leaves);
        assert!(leaves.iter().all(|file| !child.contains(file)));
        assert!(
            parent
                .clone()
                .with_isolated_readable_files(&vec![leaves[0].clone(); 17])
                .is_err()
        );
        assert!(
            parent
                .clone()
                .with_isolated_readable_files(&[leaves[0].clone(), leaves[0].clone()])
                .is_err()
        );
        assert!(
            SandboxPolicy::new(SandboxMode::ReadOnly, &[])
                .unwrap()
                .with_isolated_readable_files(&leaves)
                .is_err()
        );
        assert!(
            SandboxPolicy::isolated(SandboxMode::WorkspaceWrite, files.path(), &[])
                .unwrap()
                .with_isolated_readable_files(&leaves)
                .is_err()
        );
        assert!(
            SandboxPolicy::isolated(SandboxMode::FullAccess, files.path(), &[])
                .unwrap()
                .with_isolated_readable_files(&leaves)
                .is_err()
        );
    }

    #[test]
    fn exact_readable_files_allow_only_fixed_absent_gate_leaves() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let root = home.path().canonicalize().unwrap();
        let gate_parent = root.join(".polaris");
        let leaves = [
            gate_parent.join("mcp-entry-gate-v2.json"),
            gate_parent.join("mcp-entry-gate-v2.lock"),
            gate_parent.join("mcp-entry-identities-v2.json"),
        ];
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, workspace.path(), &[]).unwrap();
        // The physical HOME is enough when the fixed .polaris parent is absent.
        let absent = policy
            .clone()
            .with_isolated_readable_files(&leaves)
            .unwrap();
        assert!(absent.restrict(SandboxMode::ReadOnly, &[]).is_ok());
        std::fs::create_dir(&gate_parent).unwrap();
        assert!(policy.clone().with_isolated_readable_files(&leaves).is_ok());
        for file in [
            gate_parent.join("unapproved.json"),
            root.join("mcp-entry-gate-v2.json"),
            root.join("missing-home/.polaris/mcp-entry-gate-v2.json"),
            gate_parent.join("mcp-entry-gate-v2.json/child"),
        ] {
            assert!(
                policy
                    .clone()
                    .with_isolated_readable_files(&[file])
                    .is_err()
            );
        }
        std::fs::write(&leaves[0], b"gate").unwrap();
        assert!(policy.with_isolated_readable_files(&leaves).is_ok());
    }

    #[test]
    fn exact_readable_files_reject_unsafe_spellings_and_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().canonicalize().unwrap();
        let file = root.join("leaf");
        std::fs::write(&file, b"data").unwrap();
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, workspace.path(), &[]).unwrap();
        for path in [
            PathBuf::from("leaf"),
            PathBuf::from("/"),
            root.clone(),
            PathBuf::from(format!("{}/./leaf", root.display())),
            PathBuf::from(format!("{}//leaf", root.display())),
            PathBuf::from(format!("{}/leaf/", root.display())),
            root.join("../leaf"),
            root.join("control\nleaf"),
            root.join("x".repeat(4097)),
        ] {
            assert!(
                policy
                    .clone()
                    .with_isolated_readable_files(&[path])
                    .is_err()
            );
        }
        assert!(policy.with_isolated_readable_files(&[file]).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn exact_readable_files_reject_symlink_leaf_and_ancestors_on_registration_and_restrict() {
        use std::os::unix::fs::symlink;
        let workspace = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let root = files.path().canonicalize().unwrap();
        let physical = root.join("physical");
        std::fs::create_dir(&physical).unwrap();
        let file = physical.join("leaf");
        std::fs::write(&file, b"data").unwrap();
        let alias = root.join("alias");
        let leaf_alias = root.join("leaf-alias");
        symlink(&physical, &alias).unwrap();
        symlink(&file, &leaf_alias).unwrap();
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, workspace.path(), &[]).unwrap();
        for path in [
            alias.join("leaf"),
            leaf_alias,
            alias.join(".polaris/mcp-entry-gate-v2.json"),
        ] {
            assert!(
                policy
                    .clone()
                    .with_isolated_readable_files(&[path])
                    .is_err()
            );
        }
        let parent = policy
            .with_isolated_readable_files(std::slice::from_ref(&file))
            .unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(parent.restrict(SandboxMode::ReadOnly, &[]).is_err());
        std::fs::create_dir(&file).unwrap();
        assert!(parent.restrict(SandboxMode::ReadOnly, &[]).is_err());
        std::fs::remove_dir(&file).unwrap();
        symlink("/dev/null", &file).unwrap();
        assert!(parent.restrict(SandboxMode::ReadOnly, &[]).is_err());
    }

    #[test]
    fn exact_readable_files_reject_scratch_overlap_in_both_configuration_orders() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let sibling_home = root.join("home");
        let sibling_tmp = root.join("tmp");
        let child_home = workspace.join("home");
        let child_tmp = workspace.join("tmp");
        for dir in [
            &workspace,
            &sibling_home,
            &sibling_tmp,
            &child_home,
            &child_tmp,
        ] {
            std::fs::create_dir(dir).unwrap();
        }
        for (home, tmpdir, sibling) in [
            (&sibling_home, &sibling_tmp, true),
            (&child_home, &child_tmp, false),
        ] {
            for scratch in [home, tmpdir] {
                let file = scratch.join("leaf");
                std::fs::write(&file, b"data").unwrap();
                let policy =
                    SandboxPolicy::isolated(SandboxMode::ReadOnly, &workspace, &[]).unwrap();
                let registered = policy
                    .clone()
                    .with_isolated_readable_files(std::slice::from_ref(&file))
                    .unwrap();
                let env_first = if sibling {
                    assert!(
                        registered
                            .with_isolated_sibling_environment(home, tmpdir)
                            .is_err()
                    );
                    policy
                        .with_isolated_sibling_environment(home, tmpdir)
                        .unwrap()
                } else {
                    assert!(registered.with_isolated_environment(home, tmpdir).is_err());
                    policy.with_isolated_environment(home, tmpdir).unwrap()
                };
                assert!(env_first.with_isolated_readable_files(&[file]).is_err());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_readable_files_reject_private_container_overlap_in_both_configuration_orders() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let container = private.path().canonicalize().unwrap();
        let home = container.join("home");
        let tmpdir = container.join("tmp");
        for dir in [&container, &home, &tmpdir] {
            if dir != &container {
                std::fs::create_dir(dir).unwrap();
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let file = container.join("outside-scratch-leaf");
        std::fs::write(&file, b"data").unwrap();
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, workspace.path(), &[]).unwrap();
        let file_first = policy
            .clone()
            .with_isolated_readable_files(std::slice::from_ref(&file))
            .unwrap();
        assert!(
            file_first
                .with_isolated_private_environment(&container, &home, &tmpdir)
                .is_err()
        );
        let env_first = policy
            .with_isolated_private_environment(&container, &home, &tmpdir)
            .unwrap();
        assert!(env_first.with_isolated_readable_files(&[file]).is_err());
    }

    #[test]
    fn runtime_bins_require_disjoint_readable_directories_and_valid_path_elements() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let home = root.join("home");
        let tmp = root.join("tmp");
        let runtime = root.join("runtime");
        let bin = runtime.join("bin");
        let outside = root.join("outside");
        let colon = runtime.join("bad:path");
        for dir in [&workspace, &home, &tmp, &runtime, &bin, &outside, &colon] {
            std::fs::create_dir(dir).unwrap();
        }
        let policy = SandboxPolicy::isolated(
            SandboxMode::FullAccess,
            &workspace,
            std::slice::from_ref(&runtime),
        )
        .unwrap()
        .with_isolated_sibling_environment(&home, &tmp)
        .unwrap();
        for invalid in [
            &workspace,
            &home,
            &tmp,
            &outside,
            &root,
            &colon,
            &PathBuf::from("."),
        ] {
            assert!(
                policy
                    .clone()
                    .with_isolated_runtime_bins(std::slice::from_ref(invalid))
                    .is_err()
            );
        }
        assert!(
            policy
                .clone()
                .with_isolated_runtime_bins(&vec![bin.clone(); 33])
                .is_err()
        );
        let configured = policy
            .with_isolated_runtime_bins(std::slice::from_ref(&bin))
            .unwrap();
        assert!(!configured.contains(&bin));
        std::fs::remove_dir(&bin).unwrap();
        assert!(configured.restrict(SandboxMode::ReadOnly, &[]).is_err());
    }

    #[test]
    fn fixed_recipe_environment_cannot_replace_identity_or_inherit_credentials() {
        use std::collections::BTreeMap;
        let fixed = BTreeMap::from([
            ("CARGO_NET_OFFLINE".into(), "true".into()),
            ("CARGO_HOME".into(), "/private/tmp/envkit/home/cargo".into()),
            ("RUSTC_WRAPPER".into(), String::new()),
            (
                "CARGO_ENCODED_RUSTFLAGS".into(),
                "-C\u{1f}link-arg=-fuse-ld=/native/ld".into(),
            ),
        ]);
        assert!(validate_fixed_values(&fixed).is_ok());
        for key in [
            "HOME",
            "PATH",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "DYLD_INSERT_LIBRARIES",
            "OPENAI_API_KEY",
        ] {
            assert!(
                validate_fixed_values(&BTreeMap::from([(key.into(), "value".into())])).is_err()
            );
        }
        for (key, value) in [
            ("GOPROXY", "https://proxy.example"),
            ("SDKROOT", "relative"),
            ("CARGO_HOME", "/tmp/a\nb"),
            ("RUSTC_WRAPPER", "/tmp/wrapper"),
            ("CARGO_ENCODED_RUSTFLAGS", "-C\nflag"),
        ] {
            assert!(validate_fixed_values(&BTreeMap::from([(key.into(), value.into())])).is_err());
        }
    }

    #[test]
    fn private_scratch_siblings_require_explicit_layout_and_preserve_read_only_source() {
        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let home = container.path().join("home");
        let tmpdir = container.path().join("tmp");
        for directory in [&workspace, &home, &tmpdir] {
            std::fs::create_dir(directory).unwrap();
        }
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, &workspace, &[]).unwrap();
        assert!(
            policy
                .clone()
                .with_isolated_environment(&home, &tmpdir)
                .is_err()
        );
        let policy = policy
            .with_isolated_sibling_environment(&home, &tmpdir)
            .unwrap();
        assert!(!policy.contains(&workspace));
        assert!(!policy.contains(&home));
        let child = policy.restrict(SandboxMode::ReadOnly, &[]).unwrap();
        assert_eq!(
            child
                .isolated_boundary()
                .unwrap()
                .environment
                .as_ref()
                .unwrap()
                .home,
            home.canonicalize().unwrap()
        );
        let other = tempfile::tempdir().unwrap();
        assert!(
            policy
                .with_isolated_sibling_environment(other.path(), &tmpdir)
                .is_err()
        );
    }

    #[test]
    fn isolated_environment_is_explicit_and_inherited_without_source_write_grants() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let tmpdir = root.path().join("tmp");
        for path in [&home, &tmpdir] {
            std::fs::create_dir(path).unwrap();
        }
        let parent = SandboxPolicy::isolated(SandboxMode::FullAccess, root.path(), &[])
            .unwrap()
            .with_isolated_environment(&home, &tmpdir)
            .unwrap();
        let reader = parent.restrict(SandboxMode::ReadOnly, &[]).unwrap();
        let environment = reader
            .isolated_boundary()
            .unwrap()
            .environment
            .as_ref()
            .unwrap();
        assert_eq!(environment.home, home.canonicalize().unwrap());
        assert_eq!(environment.tmpdir, tmpdir.canonicalize().unwrap());
        assert_eq!(reader.mode(), SandboxMode::ReadOnly);
        assert!(reader.writable_roots().is_empty());
        assert!(!reader.contains(&environment.home));
        assert!(!reader.contains(&root.path().join("source.txt")));
        std::fs::remove_dir(&tmpdir).unwrap();
        assert!(reader.restrict(SandboxMode::ReadOnly, &[]).is_err());
    }

    #[test]
    fn isolated_environment_rejects_broad_external_missing_and_overlapping_paths() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let nested = home.join("nested");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&nested).unwrap();
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, root.path(), &[]).unwrap();
        for path in [
            root.path().to_path_buf(),
            outside.path().to_path_buf(),
            root.path().join("missing"),
            home.clone(),
            nested,
        ] {
            assert!(
                policy
                    .clone()
                    .with_isolated_environment(&home, &path)
                    .is_err()
            );
        }
        assert!(
            SandboxPolicy::new(SandboxMode::ReadOnly, &[])
                .unwrap()
                .with_isolated_environment(&home, outside.path())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn isolated_environment_rejects_scratch_rebound_to_external_alias() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let tmpdir = root.path().join("tmp");
        for path in [&home, &tmpdir] {
            std::fs::create_dir(path).unwrap();
        }
        let policy = SandboxPolicy::isolated(SandboxMode::ReadOnly, root.path(), &[])
            .unwrap()
            .with_isolated_environment(&home, &tmpdir)
            .unwrap();
        std::fs::remove_dir(&tmpdir).unwrap();
        std::os::unix::fs::symlink(outside.path(), &tmpdir).unwrap();
        assert!(policy.restrict(SandboxMode::ReadOnly, &[]).is_err());
        assert!(policy.with_isolated_environment(&home, &tmpdir).is_err());
    }

    #[test]
    fn isolated_restrict_preserves_reads_and_cwd_but_only_narrows_writes() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let selected = root.join("selected");
        std::fs::create_dir(&selected).unwrap();
        for mode in [SandboxMode::WorkspaceWrite, SandboxMode::FullAccess] {
            let parent = SandboxPolicy::isolated(mode, &root, &[runtime.path().into()]).unwrap();
            let child = parent
                .restrict(mode, std::slice::from_ref(&selected))
                .unwrap();
            let boundary = child.isolated_boundary().unwrap();
            assert_eq!(boundary.workspace, root);
            assert_eq!(
                boundary.readable_roots,
                parent.isolated_boundary().unwrap().readable_roots
            );
            assert!(child.contains(&selected.join("new.txt")));
            assert!(!child.contains(&root.join("sibling.txt")));
            assert!(!child.contains(runtime.path()));
            assert!(child.restrict(mode, std::slice::from_ref(&root)).is_err());
            assert!(child.restrict(mode, &[runtime.path().into()]).is_err());
            let reader = child.restrict(SandboxMode::ReadOnly, &[]).unwrap();
            assert_eq!(reader.isolated_boundary().unwrap().workspace, root);
            assert_eq!(
                reader.isolated_boundary().unwrap().readable_roots,
                boundary.readable_roots
            );
            assert!(reader.writable_roots().is_empty());
            assert!(!reader.contains(&selected));
            assert!(
                reader
                    .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&selected))
                    .is_err()
            );
        }
    }

    #[test]
    fn restrict_rejects_invalid_shapes_and_mode_elevation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let parent = SandboxPolicy::isolated(SandboxMode::WorkspaceWrite, &root, &[]).unwrap();
        assert!(
            parent
                .restrict(SandboxMode::FullAccess, std::slice::from_ref(&root))
                .is_err()
        );
        assert!(
            parent
                .restrict(SandboxMode::ReadOnly, std::slice::from_ref(&root))
                .is_err()
        );
        assert!(parent.restrict(SandboxMode::WorkspaceWrite, &[]).is_err());
        assert!(
            parent
                .restrict(SandboxMode::WorkspaceWrite, &[root.join("missing")])
                .is_err()
        );
        let full = SandboxPolicy::isolated(SandboxMode::FullAccess, &root, &[]).unwrap();
        assert!(full.restrict(SandboxMode::FullAccess, &[]).is_err());
    }

    #[test]
    fn isolated_restrict_rejects_missing_inherited_roots() {
        for missing_runtime in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().join("workspace");
            let runtime = dir.path().join("runtime");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::create_dir(&runtime).unwrap();
            let parent = SandboxPolicy::isolated(
                SandboxMode::FullAccess,
                &workspace,
                std::slice::from_ref(&runtime),
            )
            .unwrap();
            assert!(parent.restrict(SandboxMode::ReadOnly, &[]).is_ok());
            std::fs::remove_dir(if missing_runtime {
                &runtime
            } else {
                &workspace
            })
            .unwrap();
            assert!(parent.restrict(SandboxMode::ReadOnly, &[]).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn isolated_restrict_rejects_rebound_roots_and_escaping_child_aliases() {
        for replace_runtime in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().join("workspace");
            let runtime = dir.path().join("runtime");
            let outside = dir.path().join("outside");
            for path in [&workspace, &runtime, &outside] {
                std::fs::create_dir(path).unwrap();
            }
            let parent = SandboxPolicy::isolated(
                SandboxMode::FullAccess,
                &workspace,
                std::slice::from_ref(&runtime),
            )
            .unwrap();
            let alias = workspace.join("escape");
            std::os::unix::fs::symlink(&outside, &alias).unwrap();
            assert!(
                parent
                    .restrict(SandboxMode::WorkspaceWrite, &[alias])
                    .is_err()
            );
            let replaced = if replace_runtime {
                &runtime
            } else {
                &workspace
            };
            std::fs::rename(replaced, dir.path().join("old")).unwrap();
            std::os::unix::fs::symlink(&outside, replaced).unwrap();
            assert!(parent.restrict(SandboxMode::ReadOnly, &[]).is_err());
        }
    }

    #[test]
    fn restrict_keeps_legacy_constructor_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let full = SandboxPolicy::new(SandboxMode::FullAccess, &[]).unwrap();
        let full_child = full.restrict(SandboxMode::FullAccess, &[]).unwrap();
        assert!(full_child.contains(Path::new("/arbitrary")));
        assert!(full_child.isolated_boundary().is_none());
        assert!(
            full.restrict(SandboxMode::FullAccess, std::slice::from_ref(&root))
                .is_err()
        );
        let writer = full
            .restrict(SandboxMode::WorkspaceWrite, std::slice::from_ref(&root))
            .unwrap();
        assert!(writer.contains(&root.join("new")));
        assert!(!writer.contains(root.parent().unwrap()));
        assert!(writer.isolated_boundary().is_none());
        assert!(writer.restrict(SandboxMode::FullAccess, &[]).is_err());
        let reader = writer.restrict(SandboxMode::ReadOnly, &[]).unwrap();
        assert!(!reader.contains(&root));
        assert!(reader.isolated_boundary().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_root_reached_through_a_symlink_is_stored_canonicalised() {
        // macOS's /tmp is a symlink to /private/tmp, and tempfile::tempdir()
        // is also created under it. Without normalization, the enforcement
        // side judges the given root and the actual path to not match and
        // falls back to "deny everything". Worse, that denial is
        // indistinguishable from a policy-violation denial, so an
        // acceptance-criteria test would pass for the wrong reason.
        let real = tempfile::tempdir().expect("temp dir");
        let link_parent = tempfile::tempdir().expect("temp dir");
        let link = link_parent.path().join("link-to-root");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&link))
            .expect("can't create policy");

        let stored = &policy.writable_roots()[0];
        assert_eq!(
            stored,
            &real.path().canonicalize().expect("canonicalize"),
            "the root was not normalized. The stored value {} is still the link",
            stored.display()
        );
        assert_ne!(stored, &link, "the link's path was stored verbatim");
    }

    #[test]
    fn workspace_write_requires_at_least_one_root() {
        // A workspace-write with zero roots means "can't write anywhere",
        // but that's indistinguishable from a caller's construction
        // omission. Don't silently accept a state that can't be told apart.
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[])
            .expect_err("zero roots went through");
        assert!(
            matches!(err, SandboxError::NotEnforced(_)),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_only_rejects_writable_roots() {
        // Letting a root be passed to read-only would make the policy's
        // name and its actual permissions disagree. That violates the
        // design goal of making the declaration and the enforcement the
        // same object.
        let dir = tempfile::tempdir().expect("temp dir");
        let err = SandboxPolicy::new(SandboxMode::ReadOnly, &[dir.path().to_path_buf()])
            .expect_err("a root went through for read-only");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn a_nonexistent_root_is_rejected_rather_than_silently_dropped() {
        // A nonexistent root can't be canonicalized. Silently dropping it
        // would mean nobody notices that a place they thought they could
        // write to has quietly gone missing.
        let missing = std::path::PathBuf::from("/definitely/not/here/polaris-test");
        let err = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[missing])
            .expect_err("a nonexistent root went through");
        assert!(matches!(err, SandboxError::Io(_)), "{err}");
    }

    #[test]
    fn full_access_needs_no_roots_and_keeps_none() {
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("can't create full-access");
        assert_eq!(policy.mode(), SandboxMode::FullAccess);
        assert!(policy.writable_roots().is_empty());
    }

    #[test]
    fn describe_names_the_mode_and_every_root() {
        // The denial message includes this string. Since the spec requires
        // "the denied path, the current policy, the writable roots", any
        // formatting that drops even one root would hand the model a wrong
        // map.
        let a = tempfile::tempdir().expect("temp dir");
        let b = tempfile::tempdir().expect("temp dir");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            &[a.path().to_path_buf(), b.path().to_path_buf()],
        )
        .expect("can't create policy");

        let s = policy.describe();
        assert!(s.contains("workspace-write"), "policy name is missing: {s}");
        for root in policy.writable_roots() {
            assert!(
                s.contains(&root.display().to_string()),
                "root {} is missing from the description: {s}",
                root.display()
            );
        }
    }

    #[test]
    fn full_access_rejects_writable_roots() {
        // Letting a root be passed to full-access would make the policy's
        // name and its actual permissions disagree. Reject it the same way
        // as read-only.
        let dir = tempfile::tempdir().expect("temp dir");
        let err = SandboxPolicy::new(SandboxMode::FullAccess, &[dir.path().to_path_buf()])
            .expect_err("a root went through for full-access");
        assert!(matches!(err, SandboxError::NotEnforced(_)), "{err}");
    }

    #[test]
    fn contains_includes_root_and_child_paths() {
        // The root itself, and child directories/files under it, are
        // contained. Sibling directories are not.
        let root = tempfile::tempdir().expect("temp dir");
        let root_path = root.path().canonicalize().expect("canonicalize");
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&root_path.clone()),
        )
        .expect("can't create policy");

        // The root itself is contained
        assert!(
            policy.contains(&root_path),
            "root itself should be contained"
        );

        // A child directory is contained
        let child = root_path.join("child");
        assert!(
            policy.contains(&child),
            "child directory should be contained"
        );

        // A sibling directory is not contained
        let sibling = root_path.parent().unwrap().join("sibling");
        assert!(
            !policy.contains(&sibling),
            "sibling directory should not be contained"
        );
    }

    #[test]
    fn contains_full_access_always_true() {
        // full-access returns true for any path.
        let policy =
            SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("can't create full-access");

        let arbitrary = Path::new("/arbitrary/path");
        assert!(
            policy.contains(arbitrary),
            "full-access should contain any path"
        );
    }

    #[test]
    fn contains_read_only_always_false() {
        // read-only returns false for any path.
        let policy =
            SandboxPolicy::new(SandboxMode::ReadOnly, &[]).expect("can't create read-only");

        let arbitrary = Path::new("/arbitrary/path");
        assert!(
            !policy.contains(arbitrary),
            "read-only should contain no paths"
        );
    }

    #[test]
    fn contains_resists_string_prefix_confusion() {
        // Path::starts_with compares by path component. <root>/work and
        // <root>/work-evil are different. This test guards against the
        // comparison being "simplified" into a plain string comparison.
        let root = tempfile::tempdir().expect("temp dir");
        let root_path = root.path().canonicalize().expect("canonicalize");

        // Actually create the work directory
        let work = root_path.join("work");
        std::fs::create_dir(&work).expect("create work dir");
        let work = work.canonicalize().expect("canonicalize work");

        // work-evil exists under the root, but isn't the root's path
        let work_evil = root_path.join("work-evil");

        let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&work))
            .expect("can't create policy");

        // Everything under work is contained
        assert!(policy.contains(&work), "work should be contained");
        let work_child = work.join("file.txt");
        assert!(
            policy.contains(&work_child),
            "work/child should be contained"
        );

        // work-evil is not contained
        assert!(
            !policy.contains(&work_evil),
            "work-evil should not be contained (string prefix confusion)"
        );
    }
}
