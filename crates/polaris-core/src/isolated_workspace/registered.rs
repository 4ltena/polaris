//! Admission of a host-registered directory without copying or modifying it.
//! Like snapshots, this does not freeze descendants against an unsandboxed peer.
use super::*;

/// A dedicated, private cwd. Keep this owner and revalidate immediately before
/// spawning. Ordinary child output may change between runs; unsafe entries may
/// not. Unlike snapshotting, exclusions reject the whole directory.
#[derive(Debug)]
pub struct RegisteredDirectory {
    path: PathBuf,
    root: File,
    ancestors: Vec<(PathBuf, Identity)>,
    limits: Limits,
}

impl RegisteredDirectory {
    pub fn open(path: &Path, limits: Limits) -> Result<Self> {
        let (root, ancestors) = open_root_with_ancestors(path)?;
        let value = Self {
            path: path.into(),
            root,
            ancestors,
            limits,
        };
        value.revalidate()?;
        Ok(value)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn revalidate(&self) -> Result<()> {
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets = RegisteredSecrets::from_auth_registry(registered)?;
            self.check(&secrets)
        })
        .map_err(|_| error(&self.path, ErrorKind::ProtectionUnavailable))?
    }

    fn check(&self, secrets: &RegisteredSecrets) -> Result<()> {
        let (current, ancestors) = open_root_with_ancestors(&self.path)?;
        let before = io(&self.path, current.metadata())?;
        if self.ancestors != ancestors
            || Identity::of(&before) != Identity::of(&io(&self.path, self.root.metadata())?)
        {
            return Err(error(&self.path, ErrorKind::SourceChanged));
        }
        if self.path.parent().is_none()
            || before.uid() != unsafe { libc::geteuid() }
            || before.mode() & 0o7077 != 0
            || secrets.denies(&self.path, &before)
            || secrets.paths.iter().any(|p| p.starts_with(&self.path))
            || polaris_tools::path_policy::is_denied(&self.path)
            || is_repository_metadata(&self.path)
        {
            return Err(error(&self.path, ErrorKind::InvalidPath));
        }
        let mut walker = Copier {
            secrets,
            private_owner: Some(before.uid()),
            limits: self.limits,
            entries: 0,
            files: 0,
            bytes: 0,
            root_path: &self.path,
            policy_root: &self.path,
            destination: None,
            selection: None,
            manifest: Vec::new(),
            exclusions: Vec::new(),
            deadline: None,
            exclude_build_caches: false,
        };
        walker.walk(&current, Path::new(""), 0)?;
        if !walker.exclusions.is_empty() {
            return Err(error(&self.path, ErrorKind::InvalidPath));
        }
        let (after, ancestors) = open_root_with_ancestors(&self.path)?;
        if ancestors != self.ancestors || !unchanged(&before, &io(&self.path, after.metadata())?) {
            return Err(error(&self.path, ErrorKind::SourceChanged));
        }
        Ok(())
    }
}
