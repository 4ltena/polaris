//! Descriptor-pinned download staging; cleanup touches only this transaction.
use super::*;
use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd},
};

pub(super) struct Staging {
    chain: Vec<(PathBuf, File)>,
    directory: File,
    name: String,
}

impl Staging {
    pub(super) fn new(
        home: &Path,
        path: &Path,
        required: u64,
    ) -> Result<Self, ManagedDecisionError> {
        use ManagedDecisionError::{Install, Integrity};
        if !home.is_absolute() || home.canonicalize().ok().as_deref() != Some(home) {
            return Err(Integrity);
        }
        let root = crate::project_mutation::open_directory_nofollow(home).map_err(|_| Integrity)?;
        let mut chain = vec![(home.to_owned(), root)];
        // Verify the home before creating any children under it.
        install::verify_chain(&chain)?;
        for name in [".polaris", "decision-runtime", "downloads"] {
            install::create_child(&mut chain, name)?;
        }
        install::verify_chain(&chain)?;
        let (parent_path, parent) = chain.last().ok_or(Integrity)?;
        if path.parent() != Some(parent_path) {
            return Err(Integrity);
        }
        let name = path.file_name().and_then(|v| v.to_str()).ok_or(Integrity)?;
        if name.len() != 41
            || !name.starts_with(".pending-")
            || !name[9..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Integrity);
        }
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(parent.as_raw_fd(), info.as_mut_ptr()) } != 0 {
            return Err(Install);
        }
        let info = unsafe { info.assume_init() };
        if u64::from(info.f_bavail)
            .checked_mul(info.f_frsize)
            .ok_or(Integrity)?
            < required
        {
            return Err(ManagedDecisionError::InsufficientSpace);
        }
        let cname = CString::new(name).map_err(|_| Integrity)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), cname.as_ptr(), 0o700) } != 0 {
            return Err(ManagedDecisionError::Conflict);
        }
        let directory =
            polaris_tools::path_policy::tree_admission::open_at(parent, name.as_ref(), true, path)
                .map_err(|_| Integrity)?;
        private(&directory)?;
        Ok(Self {
            chain,
            directory,
            name: name.into(),
        })
    }

    pub(super) fn verify(&self) -> Result<(), ManagedDecisionError> {
        install::verify_chain(&self.chain)?;
        private(&self.directory)?;
        install::same_directory(
            &self.chain.last().ok_or(ManagedDecisionError::Integrity)?.1,
            &self.name,
            &self.directory,
        )
    }

    pub(super) fn create(&self, name: &str) -> Result<File, ManagedDecisionError> {
        self.verify()?;
        if !["worker", "model.gguf"].contains(&name) {
            return Err(ManagedDecisionError::Integrity);
        }
        let name = CString::new(name).map_err(|_| ManagedDecisionError::Integrity)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(ManagedDecisionError::Install);
        }
        // SAFETY: this newly owned descriptor is transferred once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let Some((_, parent)) = self.chain.last() else {
            return;
        };
        if install::same_directory(parent, &self.name, &self.directory).is_err() {
            return;
        }
        for leaf in [c"worker", c"model.gguf"] {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), leaf.as_ptr(), 0);
            }
        }
        if let Ok(name) = CString::new(self.name.as_str()) {
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
            }
            let _ = parent.sync_all();
        }
    }
}
