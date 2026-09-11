//! Explicit, bounded copies of already relocatable packages. No discovery,
//! symlink following, dependency resolution, host caches, or relocation occurs.
//! The trusted owner selects packages; model output must not construct this plan.

use super::*;

/// A package must work at a new absolute prefix and contain no symlinks, hardlinks,
/// secrets, Git metadata, or special files. Bin paths are relative directories.
#[derive(Clone, Debug)]
pub struct RelocatablePackage {
    pub root: PathBuf,
    pub bin_dirs: Vec<PathBuf>,
}

/// Bounds apply to the whole plan, independently of workspace/helper limits.
/// All limits are mandatory; zero allows no corresponding resource.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeLimits {
    pub max_packages: usize,
    pub max_bin_dirs: usize,
    pub copy: Limits,
}

#[derive(Clone, Debug)]
pub struct ToolchainPlan {
    pub packages: Vec<RelocatablePackage>,
    pub limits: RuntimeLimits,
}

#[derive(Default)]
pub(super) struct StagedToolchains {
    packages: Vec<CopiedPackage>,
    pub(super) bin_dirs: Vec<PathBuf>,
}

impl StagedToolchains {
    pub(super) fn roots(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.packages.iter().map(|p| p.snapshot.path().to_owned())
    }
}

struct CopiedPackage {
    snapshot: Snapshot,
    // Teardown uses the pinned destination, never the original package path.
    root: File,
}

impl CopiedPackage {
    fn seal(&self) -> Result<()> {
        for entry in &self.snapshot.manifest {
            let directory = entry.sha256.is_none();
            let file = open_relative(&self.root, &entry.relative_path, directory)?;
            let mode = if directory || entry.mode & 0o111 != 0 {
                0o500
            } else {
                0o400
            };
            io(file.set_permissions(Permissions::from_mode(mode)))?;
        }
        io(self.root.set_permissions(Permissions::from_mode(0o500)))
    }
}

impl Drop for CopiedPackage {
    fn drop(&mut self) {
        // This also handles partially completed sealing. No source permissions
        // are changed; regular files need no write permission for unlinking.
        let _ = self.root.set_permissions(Permissions::from_mode(0o700));
        for entry in &self.snapshot.manifest {
            if entry.sha256.is_none()
                && let Ok(directory) = open_relative(&self.root, &entry.relative_path, true)
            {
                let _ = directory.set_permissions(Permissions::from_mode(0o700));
            }
        }
    }
}

fn relative_directory(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.as_os_str().len() > 4096
        || path.components().count() > 64
        || path.as_os_str().as_bytes().contains(&b':')
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(PrepareError::InvalidPath);
    }
    Ok(())
}

fn open_relative(root: &File, path: &Path, directory: bool) -> Result<File> {
    relative_directory(path)?;
    let mut file = io(root.try_clone())?;
    let mut parts = path.components().peekable();
    while let Some(Component::Normal(name)) = parts.next() {
        let name = CString::new(name.as_bytes()).map_err(|_| PrepareError::InvalidPath)?;
        file = open_at(&file, &name, parts.peek().is_some() || directory, false)?;
    }
    Ok(file)
}

// Resolve reserved, not-yet-created leaves without reading any contents.
fn reserved_physical(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut physical) => {
                for component in suffix.iter().rev() {
                    physical.push(component);
                }
                return Ok(physical);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(ancestor.file_name().ok_or(PrepareError::InvalidPath)?);
                ancestor = ancestor.parent().ok_or(PrepareError::InvalidPath)?;
            }
            Err(_) => return Err(PrepareError::Io),
        }
    }
}

pub(super) fn stage(
    plan: Option<&ToolchainPlan>,
    source: &Path,
    protected: &ProtectedPathsSnapshot,
    secrets: &RegisteredSecrets,
) -> Result<StagedToolchains> {
    let Some(plan) = plan else {
        return Ok(StagedToolchains::default());
    };
    // Keep descriptor/profile/path counts bounded even for an excessive plan.
    if plan.packages.len() > plan.limits.max_packages.min(16) {
        return Err(PrepareError::Limit);
    }
    let source_meta = io(open_physical(source, true)?.metadata())?;
    let mut bin_count = 0usize;
    for (index, package) in plan.packages.iter().enumerate() {
        physical(&package.root)?;
        let metadata = io(open_physical(&package.root, true)?.metadata())?;
        if overlaps(&package.root, source)
            || ancestor_has_identity(source, &metadata)?
            || ancestor_has_identity(&package.root, &source_meta)?
            || secrets.denies(&package.root, &metadata)
            || polaris_tools::path_policy::is_denied(&package.root)
            || plan.packages[..index]
                .iter()
                .any(|p| overlaps(&p.root, &package.root))
        {
            return Err(PrepareError::RuntimeDenied);
        }
        for secret in protected.paths() {
            if overlaps(&package.root, secret)
                || overlaps(&package.root, &reserved_physical(secret)?)
            {
                return Err(PrepareError::RuntimeDenied);
            }
        }
        bin_count = bin_count
            .checked_add(package.bin_dirs.len())
            .ok_or(PrepareError::Limit)?;
        if bin_count > plan.limits.max_bin_dirs.min(32) {
            return Err(PrepareError::Limit);
        }
        for bin in &package.bin_dirs {
            relative_directory(bin)?;
        }
    }

    let mut result = StagedToolchains::default();
    let mut remaining = plan.limits.copy;
    for package in &plan.packages {
        // Reuse the credential-serialized FD copier, including old identities,
        // no-follow opens, growth limits, hashing and source-change checks.
        let snapshot =
            isolated_workspace::snapshot(&package.root, secrets, remaining).map_err(|error| {
                match error.kind {
                    isolated_workspace::ErrorKind::Limit => PrepareError::Limit,
                    isolated_workspace::ErrorKind::SourceChanged => PrepareError::Changed,
                    _ => PrepareError::Snapshot,
                }
            })?;
        // Workspace snapshots may omit unsafe entries. A runtime must instead
        // fail as a whole: never publish a silently incomplete package.
        if !snapshot.exclusions.is_empty() {
            return Err(PrepareError::RuntimeDenied);
        }
        remaining.max_entries = remaining
            .max_entries
            .checked_sub(snapshot.manifest.len())
            .ok_or(PrepareError::Limit)?;
        for entry in &snapshot.manifest {
            if entry.sha256.is_some() {
                remaining.max_files = remaining
                    .max_files
                    .checked_sub(1)
                    .ok_or(PrepareError::Limit)?;
                remaining.max_total_bytes = remaining
                    .max_total_bytes
                    .checked_sub(entry.bytes)
                    .ok_or(PrepareError::Limit)?;
            }
        }
        let root = open_physical(snapshot.path(), true)?;
        let copied = CopiedPackage { snapshot, root };
        for bin in &package.bin_dirs {
            open_relative(&copied.root, bin, true)?;
            let path = copied.snapshot.path().join(bin);
            if !result.bin_dirs.contains(&path) {
                result.bin_dirs.push(path);
            }
        }
        copied.seal()?;
        result.packages.push(copied);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
