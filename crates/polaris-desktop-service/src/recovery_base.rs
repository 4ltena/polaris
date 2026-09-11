//! Trusted startup-only recovery placement; never a source-write grant.
use crate::{CurrentSourcePolicy, PinnedRecoveryBase};
use polaris_auth::protection::{ProtectedIdentity, ProtectedPathsSnapshot};
use polaris_core::{
    desktop_store::{BootstrapIdentity, DesktopRoot},
    workspace_apply::RecoveryParent,
};
use polaris_desktop_protocol::ids::{DecimalU64, ProjectId, SessionId};
use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Suitable,
    DifferentFilesystem,
    InsideSource,
}
/// Directory creation may have happened even though preparation failed. The
/// retained parent/child FDs select the evidence; provenance must not be reopened.
pub struct RecoveryBaseError {
    retained: Option<Retained>,
}
struct Retained {
    parent: Arc<File>,
    child: Option<File>,
    provenance: PathBuf,
}
impl RecoveryBaseError {
    pub fn may_have_created(&self) -> bool {
        self.retained.is_some()
    }
    pub fn retained_provenance(&self) -> Option<&Path> {
        self.retained.as_ref().map(|r| r.provenance.as_path())
    }
}
impl std::fmt::Debug for RecoveryBaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryBaseError")
            .field("may_have_created", &self.may_have_created())
            .finish()
    }
}
impl std::fmt::Display for RecoveryBaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("trusted recovery directory preparation failed")
    }
}
impl std::error::Error for RecoveryBaseError {}
fn refused() -> RecoveryBaseError {
    RecoveryBaseError { retained: None }
}
fn identity(file: &File) -> Result<(u64, u64), RecoveryBaseError> {
    let m = file.metadata().map_err(|_| refused())?;
    Ok((m.dev(), m.ino()))
}
fn lexical(path: &Path) -> Result<(), RecoveryBaseError> {
    let b = path.as_os_str().as_bytes();
    if b.len() > 3800
        || path.to_str().is_none()
        || b.first() != Some(&b'/')
        || b.len() == 1
        || b.contains(&0)
        || b[1..]
            .split(|v| *v == b'/')
            .any(|p| p.is_empty() || p == b"." || p == b"..")
    {
        return Err(refused());
    }
    Ok(())
}
fn open_at(parent: &File, name: &[u8]) -> Result<File, RecoveryBaseError> {
    let name = CString::new(name).map_err(|_| refused())?;
    // SAFETY: owned predecessor FD and bounded NUL-free directory component.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(refused());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
struct Anchor {
    path: PathBuf,
    chain: Vec<File>,
}
impl Anchor {
    fn open(path: &Path) -> Result<Self, RecoveryBaseError> {
        lexical(path)?;
        let mut chain = vec![File::open("/").map_err(|_| refused())?];
        for part in path.as_os_str().as_bytes()[1..].split(|b| *b == b'/') {
            chain.push(open_at(chain.last().unwrap(), part)?);
        }
        Ok(Self {
            path: path.into(),
            chain,
        })
    }
    fn file(&self) -> &File {
        self.chain.last().unwrap()
    }
    fn parent(&self) -> &File {
        &self.chain[self.chain.len() - 2]
    }
    fn check(&self) -> Result<(), RecoveryBaseError> {
        let current = Self::open(&self.path)?;
        if current.chain.len() != self.chain.len() {
            return Err(refused());
        }
        for (a, b) in self.chain.iter().zip(&current.chain) {
            if identity(a)? != identity(b)? {
                return Err(refused());
            }
        }
        Ok(())
    }
}
fn inside(directory: &File, source: &File) -> Result<bool, RecoveryBaseError> {
    let forbidden = identity(source)?;
    let mut current = directory.try_clone().map_err(|_| refused())?;
    for _ in 0..1024 {
        let here = identity(&current)?;
        if here == forbidden {
            return Ok(true);
        }
        let parent = open_at(&current, b"..")?;
        if identity(&parent)? == here {
            return Ok(false);
        }
        current = parent;
    }
    Err(refused())
}
fn placement(directory: &File, source: &File) -> Result<Placement, RecoveryBaseError> {
    if identity(directory)?.0 != identity(source)?.0 {
        return Ok(Placement::DifferentFilesystem);
    }
    if inside(directory, source)? {
        return Ok(Placement::InsideSource);
    }
    Ok(Placement::Suitable)
}
fn protected(
    registry: &ProtectedPathsSnapshot,
    path: &Path,
    file: &File,
) -> Result<(), RecoveryBaseError> {
    let (dev, ino) = identity(file)?;
    if registry
        .identities()
        .contains(&ProtectedIdentity { dev, ino })
        || registry.paths().iter().any(|p| path.starts_with(p))
    {
        return Err(refused());
    }
    Ok(())
}
/// Call only after native confirmation and bootstrap/store binding. No discovery
/// outside the supplied metadata root and the verified source's immediate parent.
/// Creates one private directory, retains it on errors/drop, never reuses names.
pub fn prepare_recovery_base(
    source: &CurrentSourcePolicy,
    metadata_root: &DesktopRoot,
    metadata_path: &Path,
    project: &ProjectId,
    session: &SessionId,
) -> Result<PinnedRecoveryBase, RecoveryBaseError> {
    prepare(
        source,
        metadata_root,
        metadata_path,
        project,
        session,
        #[cfg(test)]
        |_, _| {},
        #[cfg(test)]
        |_, _| {},
    )
}
fn prepare(
    source: &CurrentSourcePolicy,
    metadata_root: &DesktopRoot,
    metadata_path: &Path,
    project: &ProjectId,
    session: &SessionId,
    #[cfg(test)] before_mkdir: impl FnOnce(&File, &str),
    #[cfg(test)] after_mkdir: impl FnOnce(&File, &str),
) -> Result<PinnedRecoveryBase, RecoveryBaseError> {
    let source_anchor = Anchor::open(&source.source_path)?;
    if !source.read_allowed
        || identity(source_anchor.file())?
            != (
                source.source_identity.device.get(),
                source.source_identity.inode.get(),
            )
    {
        return Err(refused());
    }
    let metadata = Anchor::open(metadata_path)?;
    let expected = metadata_root.identity().map_err(|_| refused())?;
    if identity(metadata.file())? != expected {
        return Err(refused());
    }
    RecoveryParent::pin(metadata.file(), expected, metadata_path).map_err(|_| refused())?;
    let mut random = [0u8; 16];
    // SAFETY: a valid 16-byte output buffer, below getentropy's 256-byte limit.
    if unsafe { libc::getentropy(random.as_mut_ptr().cast(), random.len()) } != 0 {
        return Err(refused());
    }
    let tuple = serde_json::to_vec(&("startup-recovery-v1", project, session, random))
        .map_err(|_| refused())?;
    let name = format!(
        ".polaris-recovery-{}",
        polaris_core::conversation_state::content_hash(&tuple)
    );
    let (selected, parent_path) = match placement(metadata.file(), source_anchor.file())? {
        Placement::Suitable => (metadata.file(), metadata_path),
        Placement::DifferentFilesystem | Placement::InsideSource => {
            let parent = source_anchor.parent();
            if placement(parent, source_anchor.file())? != Placement::Suitable {
                return Err(refused());
            }
            (parent, source.source_path.parent().ok_or_else(refused)?)
        }
    };
    let selected = Arc::new(selected.try_clone().map_err(|_| refused())?);
    let selected_identity = identity(&selected)?;
    let provenance = parent_path.join(&name);
    if provenance == source.source_path {
        return Err(refused());
    }
    let component = CString::new(name.as_bytes()).map_err(|_| refused())?;
    polaris_auth::protection::with_protected_paths(|registry| {
        source_anchor.check()?;
        metadata.check()?;
        protected(registry, &source.source_path, source_anchor.file())?;
        protected(registry, parent_path, &selected)?;
        if registry
            .paths()
            .iter()
            .any(|p| provenance.starts_with(p) || p.starts_with(&provenance))
        {
            return Err(refused());
        }
        if identity(&selected)? != selected_identity
            || placement(&selected, source_anchor.file())? != Placement::Suitable
        {
            return Err(refused());
        }
        #[cfg(test)]
        before_mkdir(&selected, &name);
        // SAFETY: same verified FD as open/sync below, one exclusive component.
        // EEXIST, permissions and every other IO failure are terminal, not fallback.
        if unsafe { libc::mkdirat(selected.as_raw_fd(), component.as_ptr(), 0o700) } != 0 {
            return Err(refused());
        }
        let mut retained = Retained {
            parent: selected,
            child: None,
            provenance,
        };
        let result: Result<(u64, u64), RecoveryBaseError> = (|| {
            #[cfg(test)]
            after_mkdir(&retained.parent, &name);
            retained.child = Some(open_at(&retained.parent, name.as_bytes())?);
            let child = retained.child.as_ref().unwrap();
            let id = identity(child)?;
            RecoveryParent::pin(child, id, &retained.provenance).map_err(|_| refused())?;
            if placement(child, source_anchor.file())? != Placement::Suitable {
                return Err(refused());
            }
            protected(registry, &retained.provenance, child)?;
            child.sync_all().map_err(|_| refused())?;
            retained.parent.sync_all().map_err(|_| refused())?;
            source_anchor.check()?;
            metadata.check()?;
            if identity(&retained.parent)? != selected_identity
                || placement(child, source_anchor.file())? != Placement::Suitable
            {
                return Err(refused());
            }
            Ok(id)
        })();
        match result {
            Ok((dev, ino)) => Ok(PinnedRecoveryBase {
                directory: retained.child.take().unwrap(),
                identity: BootstrapIdentity {
                    device: DecimalU64::new(dev),
                    inode: DecimalU64::new(ino),
                },
                provenance: retained.provenance,
            }),
            Err(_) => Err(RecoveryBaseError {
                retained: Some(retained),
            }),
        }
    })
    .map_err(|_| refused())?
}
#[cfg(test)]
mod tests;
