//! Trusted OS account home lookup. Never consults or changes the environment.
use std::{
    ffi::{CStr, CString, OsString},
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
};

const MAX_LOOKUP_BYTES: usize = 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrustedHomeError {
    #[error("OS user home lookup failed")]
    Lookup,
    #[error("OS user home lookup exceeds limit")]
    Limit,
    #[error("OS user home path is invalid")]
    InvalidPath,
    #[error("OS user home directory is unavailable")]
    Unavailable,
}
fn validate_path(path: &Path) -> Result<(), TrustedHomeError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > 4096
        || !bytes.starts_with(b"/")
        || bytes.len() == 1
        || bytes.contains(&0)
        || bytes[1..]
            .split(|b| *b == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(TrustedHomeError::InvalidPath);
    }
    Ok(())
}
fn validate_physical(path: &Path) -> Result<(), TrustedHomeError> {
    validate_path(path)?;
    let mut directory = File::open("/").map_err(|_| TrustedHomeError::Unavailable)?;
    for part in path.as_os_str().as_bytes()[1..].split(|b| *b == b'/') {
        let name = CString::new(part).map_err(|_| TrustedHomeError::InvalidPath)?;
        // SAFETY: owned parent FD and one NUL-free component. No file content is
        // read; O_DIRECTORY and O_NOFOLLOW reject aliases and non-directories.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(TrustedHomeError::Unavailable);
        }
        // SAFETY: openat returned a newly owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(())
}

/// Resolve the effective OS account, including when the native launcher supplied
/// an empty environment. No HOME fallback, credential read, or settings write.
/// The returned path is validated now, not a permanently pinned capability;
/// downstream protected readers must retain their own no-follow checks.
pub fn trusted_user_home() -> Result<PathBuf, TrustedHomeError> {
    // SAFETY: geteuid takes no pointers and has no failure sentinel.
    let uid = unsafe { libc::geteuid() };
    let mut size = 1024;
    loop {
        let mut buffer = vec![0u8; size];
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: output record, pointer and byte buffer are valid for this call.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE {
            if size == MAX_LOOKUP_BYTES {
                return Err(TrustedHomeError::Limit);
            }
            size = (size * 2).min(MAX_LOOKUP_BYTES);
            continue;
        }
        if status != 0 || result.is_null() {
            return Err(TrustedHomeError::Lookup);
        }
        // SAFETY: successful lookup with a nonnull result initializes record;
        // pw_dir is a NUL-terminated string valid while buffer remains alive.
        let record = unsafe { record.assume_init() };
        if record.pw_uid != uid || record.pw_dir.is_null() {
            return Err(TrustedHomeError::Lookup);
        }
        let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes();
        // Own the bytes before the lookup buffer or passwd record is dropped.
        let home = PathBuf::from(OsString::from_vec(bytes.to_vec()));
        validate_physical(&home)?;
        return Ok(home);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lexical_home_validation_rejects_root_relative_alias_and_oversize() {
        for path in [
            "",
            "/",
            "relative",
            "/Users/../other",
            "/Users/./name",
            "/Users//name",
            "/Users/name/",
            "/Users/a\0b",
        ] {
            assert!(validate_path(Path::new(path)).is_err());
        }
        assert!(validate_path(Path::new(&format!("/{}", "x".repeat(4096)))).is_err());
        assert!(validate_path(Path::new("/Users/ordinary name")).is_ok());
        assert!(validate_path(Path::new("/Users/利用者")).is_ok());
    }
    #[test]
    fn current_os_home_is_absolute_and_physically_available_without_env_lookup() {
        let home = trusted_user_home().unwrap();
        validate_path(&home).unwrap();
        validate_physical(&home).unwrap();
        assert!(std::fs::metadata(home).unwrap().is_dir());
    }
}
