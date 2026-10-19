//! Admission of a host-registered directory without copying or modifying it.
//! Like snapshots, this does not freeze descendants against an unsandboxed peer.
use super::*;

/// A dedicated, private cwd. Keep this owner and revalidate immediately before
/// spawning. Ordinary child output may change between runs; unsafe entries may
/// not. Unlike snapshotting, exclusions reject the whole directory.
#[derive(Debug)]
pub struct RegisteredDirectory {
    path: PathBuf,
    policy_root: PathBuf,
    root: File,
    binding: RootBinding,
    limits: Limits,
}

#[derive(Debug)]
enum RootBinding {
    Live(Vec<(PathBuf, Identity)>),
    Descriptor {
        parent: File,
        name: std::ffi::OsString,
        parent_identity: Identity,
    },
}

impl RegisteredDirectory {
    pub fn open(path: &Path, limits: Limits) -> Result<Self> {
        let (root, ancestors) = open_root_with_ancestors(path)?;
        let value = Self {
            path: path.into(),
            policy_root: path.into(),
            root,
            binding: RootBinding::Live(ancestors),
            limits,
        };
        value.revalidate()?;
        Ok(value)
    }

    /// Inspect an already pinned directory under its held parent descriptor.
    /// Neither path is resolved or opened here. The caller must separately
    /// prove the parent's namespace provenance and retain that proof through
    /// its whole operation; this read witness grants no execution authority.
    pub fn inspect_descriptor(
        root: File,
        parent: File,
        name: &OsStr,
        physical_path: &Path,
        policy_root: &Path,
        limits: Limits,
    ) -> Result<Self> {
        validate_absolute(physical_path)?;
        validate_absolute(policy_root)?;
        if name.as_bytes().is_empty()
            || matches!(name.as_bytes(), b"." | b"..")
            || name.as_bytes().contains(&b'/')
            || name.as_bytes().contains(&0)
            || physical_path.file_name() != Some(name)
        {
            return Err(error(physical_path, ErrorKind::InvalidPath));
        }
        let parent_identity = Identity::of(&io(physical_path, parent.metadata())?);
        let value = Self {
            path: physical_path.into(),
            policy_root: policy_root.into(),
            root,
            binding: RootBinding::Descriptor {
                parent,
                name: name.into(),
                parent_identity,
            },
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
        let current = self.current_root()?;
        let before = io(&self.path, current.metadata())?;
        if Identity::of(&before) != Identity::of(&io(&self.path, self.root.metadata())?) {
            return Err(error(&self.path, ErrorKind::SourceChanged));
        }
        if self.path.parent().is_none()
            || self.policy_root.parent().is_none()
            || !before.is_dir()
            || before.uid() != unsafe { libc::geteuid() }
            || before.mode() & 0o7077 != 0
            || secrets.denies(&self.path, &before)
            || secrets.denies(&self.policy_root, &before)
            || secrets
                .paths
                .iter()
                .any(|p| p.starts_with(&self.path) || p.starts_with(&self.policy_root))
            || crate::path_policy::is_denied(&self.path)
            || crate::path_policy::is_denied(&self.policy_root)
            || is_repository_metadata(&self.path)
            || is_repository_metadata(&self.policy_root)
        {
            return Err(error(&self.path, ErrorKind::InvalidPath));
        }
        let mut walker = TreeWalker {
            secrets,
            private_owner: Some(before.uid()),
            limits: self.limits,
            entries: 0,
            files: 0,
            bytes: 0,
            root_path: &self.path,
            policy_root: &self.policy_root,
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
        let after = self.current_root()?;
        if !unchanged(&before, &io(&self.path, after.metadata())?) {
            return Err(error(&self.path, ErrorKind::SourceChanged));
        }
        Ok(())
    }

    fn current_root(&self) -> Result<File> {
        match &self.binding {
            RootBinding::Live(expected) => {
                let (file, ancestors) = open_root_with_ancestors(&self.path)?;
                if ancestors != *expected {
                    return Err(error(&self.path, ErrorKind::SourceChanged));
                }
                Ok(file)
            }
            RootBinding::Descriptor {
                parent,
                name,
                parent_identity,
            } => {
                if Identity::of(&io(&self.path, parent.metadata())?) != *parent_identity {
                    return Err(error(&self.path, ErrorKind::SourceChanged));
                }
                open_at(parent, name, true, &self.path)
            }
        }
    }
}
