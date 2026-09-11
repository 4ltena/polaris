//! Fixed packaged execution-helper metadata, read only by the trusted launcher.
use crate::launch_arguments::parse_sha256;
use polaris_auth::protection::{ProtectedIdentity, ProtectedPathsSnapshot};
use serde::Deserialize;
use std::{
    ffi::CString,
    fs::{File, Metadata},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};
const LIMIT: u64 = 4096;
#[derive(Debug, thiserror::Error)]
#[error("packaged execution manifest unavailable or invalid")]
pub struct PackageManifestError;
pub struct PackagedExecutionHelper {
    pub helper_path: PathBuf,
    pub sha256: [u8; 32],
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    sha256: String,
}
fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn chain(path: &Path) -> Result<Vec<File>, PackageManifestError> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(PackageManifestError);
    }
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect();
    if parts.is_empty() || parts.len() > 128 {
        return Err(PackageManifestError);
    }
    let mut files = vec![File::open("/").map_err(|_| PackageManifestError)?];
    for (i, name) in parts.iter().enumerate() {
        let name = CString::new(name.as_bytes()).map_err(|_| PackageManifestError)?;
        // SAFETY: owned predecessor FD, single NUL-free component, owned result.
        let fd = unsafe {
            libc::openat(
                files.last().unwrap().as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK
                    | if i + 1 < parts.len() {
                        libc::O_DIRECTORY
                    } else {
                        0
                    },
            )
        };
        if fd < 0 {
            return Err(PackageManifestError);
        }
        files.push(unsafe { File::from_raw_fd(fd) });
    }
    Ok(files)
}
fn package_metadata(meta: &Metadata, directory: bool) -> bool {
    meta.uid() == unsafe { libc::geteuid() }
        && meta.mode() & 0o7022 == 0
        && if directory {
            meta.is_dir()
        } else {
            meta.is_file() && meta.nlink() == 1
        }
}
fn secret(registry: &ProtectedPathsSnapshot, path: &Path, meta: &Metadata) -> bool {
    registry.paths().iter().any(|p| path.starts_with(p))
        || registry.identities().contains(&ProtectedIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        })
}
/// `current_exe` must come from the trusted caller's std::env::current_exe(),
/// never argv[0], a model path, or an environment override. Does not hash/start
/// the helper: the existing protected recipe checks its actual bytes later.
pub fn read_package_manifest(
    current_exe: &Path,
) -> Result<PackagedExecutionHelper, PackageManifestError> {
    read_manifest(
        current_exe,
        #[cfg(test)]
        || {},
    )
}
fn read_manifest(
    current_exe: &Path,
    #[cfg(test)] before_body: impl FnOnce(),
) -> Result<PackagedExecutionHelper, PackageManifestError> {
    let helpers = current_exe.parent().ok_or(PackageManifestError)?;
    let contents = helpers.parent().ok_or(PackageManifestError)?;
    if current_exe.file_name() != Some("polaris-desktop-service".as_ref())
        || helpers.file_name() != Some("Helpers".as_ref())
        || contents.file_name() != Some("Contents".as_ref())
    {
        return Err(PackageManifestError);
    }
    let manifest_path = contents.join("Resources/execution-helper.json");
    polaris_auth::protection::with_protected_paths(|registry| {
        let executable = chain(current_exe)?;
        let mut files = chain(&manifest_path)?;
        // Contents and both fixed subdirectories are trusted package metadata.
        for nodes in [&executable, &files] {
            for file in &nodes[nodes.len() - 3..nodes.len() - 1] {
                if !package_metadata(&file.metadata().map_err(|_| PackageManifestError)?, true) {
                    return Err(PackageManifestError);
                }
            }
        }
        let exec_meta = executable
            .last()
            .unwrap()
            .metadata()
            .map_err(|_| PackageManifestError)?;
        if !package_metadata(&exec_meta, false) || exec_meta.mode() & 0o111 == 0 {
            return Err(PackageManifestError);
        }
        let before = files
            .last()
            .unwrap()
            .metadata()
            .map_err(|_| PackageManifestError)?;
        if !package_metadata(&before, false)
            || before.len() > LIMIT
            || secret(registry, &manifest_path, &before)
        {
            return Err(PackageManifestError);
        }
        let observations = [&executable, &files]
            .into_iter()
            .map(|nodes| {
                nodes
                    .iter()
                    .map(|file| file.metadata().map_err(|_| PackageManifestError))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(test)]
        before_body();
        let mut body = Vec::new();
        files
            .last_mut()
            .unwrap()
            .take(LIMIT + 1)
            .read_to_end(&mut body)
            .map_err(|_| PackageManifestError)?;
        if body.len() as u64 > LIMIT
            || body.len() as u64 != before.len()
            || !same(
                &before,
                &files
                    .last()
                    .unwrap()
                    .metadata()
                    .map_err(|_| PackageManifestError)?,
            )
        {
            return Err(PackageManifestError);
        }
        // Recheck name-to-FD binding, including every ancestor; never accept a
        // replacement manifest or package selected while the pinned read ran.
        for ((path, held), observed) in [
            (current_exe, &executable),
            (manifest_path.as_path(), &files),
        ]
        .into_iter()
        .zip(observations.iter())
        {
            let current = chain(path)?;
            if current.len() != held.len() {
                return Err(PackageManifestError);
            }
            for (i, ((held, b), a)) in held
                .iter()
                .zip(current.iter())
                .zip(observed.iter())
                .enumerate()
            {
                let live = held.metadata().map_err(|_| PackageManifestError)?;
                let b = b.metadata().map_err(|_| PackageManifestError)?;
                if i + 3 >= current.len() && !same(a, &live) {
                    return Err(PackageManifestError);
                }
                if a.dev() != b.dev()
                    || a.ino() != b.ino()
                    || a.mode() != b.mode()
                    || a.uid() != b.uid()
                {
                    return Err(PackageManifestError);
                }
            }
        }
        let manifest: Manifest = serde_json::from_slice(&body).map_err(|_| PackageManifestError)?;
        if manifest.schema_version != 1 {
            return Err(PackageManifestError);
        }
        Ok(PackagedExecutionHelper {
            helper_path: helpers.join("polaris-execution-helper"),
            sha256: parse_sha256(&manifest.sha256).map_err(|_| PackageManifestError)?,
        })
    })
    .map_err(|_| PackageManifestError)?
}
#[cfg(test)]
mod tests;
