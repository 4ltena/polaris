//! Request-boundary reads of two owner-selected files. No polling or model paths.
use super::{combine, extract_always_on};
use crate::isolated_workspace::RegisteredSecrets;
use std::{
    ffi::CString,
    fs::{self, File, Metadata},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::Arc,
};

const MAX_BYTES: u64 = 256 * 1024;
fn denied() -> io::Error {
    io::Error::other("AGENTS refresh unavailable: unsafe, changed, or unreadable source")
}

/// Capability created only by the trusted host owner, before entering a run.
/// Existing ancestor identities stay pinned; missing global directories may be
/// created later. Atomic replacement of AGENTS.md itself is supported.
#[derive(Debug, Clone)]
pub struct AgentsRefresh {
    global: Option<Source>,
    project: Source,
}

#[derive(Debug, Clone)]
struct Source {
    path: PathBuf,
    anchor: Arc<File>,
    ancestors: Vec<(PathBuf, (u64, u64))>,
    remaining: Vec<std::ffi::OsString>,
}

fn identity(meta: &Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}
fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    identity(a) == identity(b)
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

// Same no-follow, nonblocking fd-relative opening boundary as isolated_workspace.
fn open_at(parent: &File, name: &std::ffi::OsStr, directory: bool) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| denied())?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: live parent descriptor and NUL-terminated name; fd is owned once.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl Source {
    fn new(path: PathBuf) -> io::Result<Self> {
        if !path.is_absolute()
            || path.as_os_str().len() > 16_384
            || path.file_name() != Some(std::ffi::OsStr::new("AGENTS.md"))
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(denied());
        }
        let components: Vec<_> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_os_string()),
                _ => None,
            })
            .collect();
        let mut anchor = File::open("/")?;
        let mut prefix = PathBuf::from("/");
        let mut ancestors = vec![(prefix.clone(), identity(&anchor.metadata()?))];
        let mut index = 0;
        while index + 1 < components.len() {
            match open_at(&anchor, &components[index], true) {
                Ok(next) => {
                    prefix.push(&components[index]);
                    ancestors.push((prefix.clone(), identity(&next.metadata()?)));
                    anchor = next;
                    index += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(_) => return Err(denied()),
            }
        }
        let source = Self {
            path,
            anchor: Arc::new(anchor),
            ancestors,
            remaining: components[index..].to_vec(),
        };
        source.verify()?;
        Ok(source)
    }

    fn verify(&self) -> io::Result<()> {
        verify_ancestors(&self.ancestors).map(drop)
    }

    fn read(&self, secrets: &RegisteredSecrets) -> io::Result<Observed> {
        self.verify()?;
        for (path, _) in &self.ancestors {
            if polaris_tools::path_policy::is_denied(path)
                || secrets.denies(path, &fs::symlink_metadata(path)?)
            {
                return Err(denied());
            }
        }
        let mut parent = self.anchor.try_clone()?;
        let mut prefix = self.ancestors.last().ok_or_else(denied)?.0.clone();
        let mut ancestors = self.ancestors.clone();
        for (index, name) in self.remaining.iter().enumerate() {
            prefix.push(name);
            let is_dir = index + 1 != self.remaining.len();
            if polaris_tools::path_policy::is_denied(&prefix) {
                return Err(denied());
            }
            let named_before = match fs::symlink_metadata(&prefix) {
                Ok(meta) => {
                    if secrets.denies(&prefix, &meta)
                        || (is_dir && !meta.is_dir())
                        || (!is_dir && (!meta.is_file() || meta.nlink() != 1))
                    {
                        return Err(denied());
                    }
                    Some(meta)
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(_) => return Err(denied()),
            };
            let file = match open_at(&parent, name, is_dir) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound && named_before.is_none() => {
                    return Ok(Observed {
                        path: prefix,
                        ancestors,
                        file: None,
                        body: String::new(),
                    });
                }
                Err(_) => return Err(denied()),
            };
            let before = file.metadata()?;
            if !named_before.as_ref().is_some_and(|m| unchanged(m, &before)) {
                return Err(denied());
            }
            if secrets.denies(&prefix, &before) {
                return Err(denied());
            }
            if is_dir {
                ancestors.push((prefix.clone(), identity(&before)));
                parent = file;
                continue;
            }
            if !before.is_file()
                || before.nlink() != 1
                || before.mode() & 0o444 == 0
                || before.len() > MAX_BYTES
            {
                return Err(denied());
            }
            // Both descriptor and named ancestors are checked before any body read.
            verify_ancestors(&ancestors)?;
            let named = fs::symlink_metadata(&self.path)?;
            if !unchanged(&before, &named) {
                return Err(denied());
            }
            let mut bytes = Vec::new();
            (&file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES
                || bytes.len() as u64 != before.len()
                || !unchanged(&before, &file.metadata()?)
            {
                return Err(denied());
            }
            let body = String::from_utf8(bytes).map_err(|_| denied())?;
            return Ok(Observed {
                path: self.path.clone(),
                ancestors,
                file: Some((file, before)),
                body,
            });
        }
        Err(denied())
    }
}

fn verify_ancestors(ancestors: &[(PathBuf, (u64, u64))]) -> io::Result<File> {
    let mut reopened = File::open("/")?;
    for (path, expected) in ancestors {
        if let Some(name) = path.file_name() {
            reopened = open_at(&reopened, name, true).map_err(|_| denied())?;
        }
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_dir()
            || identity(&meta) != *expected
            || identity(&reopened.metadata()?) != *expected
        {
            return Err(denied());
        }
    }
    Ok(reopened)
}

struct Observed {
    path: PathBuf,
    ancestors: Vec<(PathBuf, (u64, u64))>,
    file: Option<(File, Metadata)>,
    body: String,
}
impl Observed {
    fn verify(&self) -> io::Result<()> {
        let parent = verify_ancestors(&self.ancestors)?;
        let reopened = open_at(&parent, self.path.file_name().ok_or_else(denied)?, false);
        match (&self.file, reopened) {
            (None, Err(e)) if e.kind() == io::ErrorKind::NotFound => {}
            (Some((_, before)), Ok(file)) if unchanged(before, &file.metadata()?) => {}
            _ => return Err(denied()),
        }
        match (&self.file, fs::symlink_metadata(&self.path)) {
            (None, Err(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            (Some((file, before)), Ok(now))
                if unchanged(before, &now) && unchanged(before, &file.metadata()?) =>
            {
                Ok(())
            }
            _ => Err(denied()),
        }
    }
}

impl AgentsRefresh {
    /// Paths must be absolute physical paths. No canonicalization fallback or
    /// ancestor discovery; the caller chooses exactly the existing two scopes.
    pub fn new(global_agents: Option<&Path>, project_root: &Path) -> io::Result<Self> {
        Ok(Self {
            global: global_agents
                .map(|p| Source::new(p.to_path_buf()))
                .transpose()?,
            project: Source::new(project_root.join("AGENTS.md"))?,
        })
    }

    pub fn from_home(project_root: &Path) -> io::Result<Self> {
        let global = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".polaris/AGENTS.md"));
        Self::new(global.as_deref(), project_root)
    }

    /// The registry lock covers both reads and post-read checks, just as for
    /// protected workspace copies. Never reuse a cloned registry after unlock.
    pub fn read(&self) -> io::Result<String> {
        self.read_checked(|| {})
    }

    fn read_checked(&self, after_reads: impl FnOnce()) -> io::Result<String> {
        polaris_auth::protection::with_protected_paths(|registered| {
            let secrets =
                RegisteredSecrets::from_auth_registry(registered).map_err(|_| denied())?;
            let global = self.global.as_ref().map(|s| s.read(&secrets)).transpose()?;
            let project = self.project.read(&secrets)?;
            after_reads();
            if let Some(global) = &global {
                global.verify()?;
            }
            project.verify()?;
            Ok(combine(
                &global
                    .as_ref()
                    .map(|g| extract_always_on(&g.body))
                    .unwrap_or_default(),
                &extract_always_on(&project.body),
            ))
        })
        .map_err(|_| denied())?
    }
}

#[cfg(test)]
mod tests;
