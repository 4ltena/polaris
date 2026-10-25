//! Bounded extraction into a fresh private staging directory. Never use tar's
//! general unpack: reject special files, hard links, traversal and duplicates;
//! install permitted relative symlinks only after every regular write closes.
use crate::{Artifact, ExecutionContract};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("environment archive is unsafe or exceeds its approved limits")]
    Unsafe,
    #[error("environment archive I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileImage {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub executable: bool,
    pub symlink: Option<String>,
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// `destination` must not exist. Partial output is retained on failure for the
/// journal owner; this function never deletes or replaces a target directory.
#[cfg(unix)]
pub fn extract_gzip(
    archive: &Path,
    expected: &Artifact,
    destination: &Path,
    prefix: &str,
    allow_runtime_symlinks: bool,
    limits: &ExecutionContract,
) -> Result<Vec<FileImage>, ArchiveError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if limits != &ExecutionContract::default()
        || !safe_relative(Path::new(prefix))
        || Path::new(prefix).components().count() != 1
    {
        return Err(ArchiveError::Unsafe);
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut source = options.open(archive)?;
    if !source.metadata()?.is_file() || source.metadata()?.len() != expected.bytes {
        return Err(ArchiveError::Unsafe);
    }
    let mut hash = Sha256::new();
    let mut verified = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        verified = verified
            .checked_add(count as u64)
            .ok_or(ArchiveError::Unsafe)?;
        if verified > expected.bytes {
            return Err(ArchiveError::Unsafe);
        }
        hash.update(&buffer[..count]);
    }
    if verified != expected.bytes || format!("{:x}", hash.finalize()) != expected.sha256 {
        return Err(ArchiveError::Unsafe);
    }
    use std::io::{Seek, SeekFrom};
    source.seek(SeekFrom::Start(0))?;
    fs::DirBuilder::new().mode(0o700).create(destination)?;
    let root = destination.canonicalize()?;
    let mut archive = tar::Archive::new(GzDecoder::new(source));
    let mut names = BTreeSet::new();
    let mut images = Vec::new();
    let mut links = Vec::new();
    let mut total = 0u64;
    let mut count = 0u32;
    for item in archive.entries()? {
        count = count.checked_add(1).ok_or(ArchiveError::Unsafe)?;
        if count > limits.max_archive_entries {
            return Err(ArchiveError::Unsafe);
        }
        let mut entry = item?;
        let path = entry.path()?.into_owned();
        if !safe_relative(&path) {
            return Err(ArchiveError::Unsafe);
        }
        let relative = path
            .strip_prefix(prefix)
            .map_err(|_| ArchiveError::Unsafe)?;
        if relative.as_os_str().is_empty() {
            if entry.header().entry_type().is_dir() {
                continue;
            }
            return Err(ArchiveError::Unsafe);
        }
        if !safe_relative(relative) || !names.insert(relative.to_owned()) {
            return Err(ArchiveError::Unsafe);
        }
        let relative_text = relative.to_str().ok_or(ArchiveError::Unsafe)?.to_owned();
        let target = root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        } else if kind.is_file() {
            let length = entry.size();
            total = total.checked_add(length).ok_or(ArchiveError::Unsafe)?;
            if total > limits.max_unpacked_bytes {
                return Err(ArchiveError::Unsafe);
            }
            let executable = entry.header().mode()? & 0o111 != 0;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(if executable { 0o700 } else { 0o600 })
                .custom_flags(libc::O_NOFOLLOW)
                .open(&target)?;
            let mut hash = Sha256::new();
            let mut copied = 0u64;
            let mut buffer = [0; 64 * 1024];
            loop {
                let n = entry.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                copied = copied.checked_add(n as u64).ok_or(ArchiveError::Unsafe)?;
                if copied > length {
                    return Err(ArchiveError::Unsafe);
                }
                file.write_all(&buffer[..n])?;
                hash.update(&buffer[..n]);
            }
            if copied != length {
                return Err(ArchiveError::Unsafe);
            }
            file.sync_all()?;
            images.push(FileImage {
                path: relative_text,
                bytes: length,
                sha256: format!("{:x}", hash.finalize()),
                executable,
                symlink: None,
            });
        } else if kind.is_symlink() && allow_runtime_symlinks {
            let link = entry.link_name()?.ok_or(ArchiveError::Unsafe)?.into_owned();
            let resolved = resolve_link(relative.parent().unwrap_or(Path::new("")), &link)?;
            links.push((target, relative_text, link, resolved));
        } else {
            return Err(ArchiveError::Unsafe);
        }
    }
    let link_targets: BTreeMap<_, _> = links
        .iter()
        .map(|(target, _, _, resolved)| (target.clone(), root.join(resolved)))
        .collect();
    // Validate all chains before creating any link. No directory links, cycles
    // or root escapes; no later archive write can traverse a created link.
    for target in link_targets.values() {
        validate_link_chain(target, &link_targets)?;
    }
    for (target, relative, link, _) in links {
        let text = link.to_str().ok_or(ArchiveError::Unsafe)?.to_owned();
        std::os::unix::fs::symlink(&link, target)?;
        images.push(FileImage {
            path: relative,
            bytes: 0,
            sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
            executable: false,
            symlink: Some(text),
        });
    }
    File::open(&root)?.sync_all()?;
    images.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(images)
}

fn validate_link_chain(
    start: &Path,
    links: &BTreeMap<PathBuf, PathBuf>,
) -> Result<(), ArchiveError> {
    let mut current = start;
    let mut seen = BTreeSet::new();
    for _ in 0..32 {
        if !seen.insert(current.to_owned()) {
            return Err(ArchiveError::Unsafe);
        }
        if let Some(next) = links.get(current) {
            current = next;
        } else {
            return if fs::symlink_metadata(current)?.is_file() {
                Ok(())
            } else {
                Err(ArchiveError::Unsafe)
            };
        }
    }
    Err(ArchiveError::Unsafe)
}

#[derive(Debug)]
pub struct ComponentImage {
    pub directory: PathBuf,
    pub files: Vec<FileImage>,
}

/// An installer archive may contain several components. Extract the verified
/// whole archive into a fresh private staging root and return only the bounded
/// selected subtree. The owner merges it create-only into its stable runtime.
#[cfg(unix)]
pub fn extract_component(
    archive: &Path,
    expected: &Artifact,
    destination: &Path,
    component: &crate::definition::RuntimeComponent,
) -> Result<ComponentImage, ArchiveError> {
    if component.artifact.0 != expected.sha256
        || !crate::definition::relative(&component.strip_prefix)
        || component.archive != crate::definition::ArchiveFormat::TarGzip
    {
        return Err(ArchiveError::Unsafe);
    }
    let (outer, inside) = component
        .strip_prefix
        .split_once('/')
        .unwrap_or((&component.strip_prefix, ""));
    let images = extract_gzip(
        archive,
        expected,
        destination,
        outer,
        true,
        &ExecutionContract::default(),
    )?;
    let directory = if inside.is_empty() {
        destination.to_owned()
    } else {
        destination.join(inside)
    };
    if directory.canonicalize()? != directory || !fs::symlink_metadata(&directory)?.is_dir() {
        return Err(ArchiveError::Unsafe);
    }
    let mut files = Vec::new();
    for mut image in images {
        let relative = Path::new(&image.path);
        let relative = if inside.is_empty() {
            relative
        } else {
            let Ok(path) = relative.strip_prefix(inside) else {
                continue;
            };
            path
        };
        if !safe_relative(relative) {
            return Err(ArchiveError::Unsafe);
        }
        if let Some(link) = &image.symlink {
            // A link safe in the original archive must remain safe after the
            // component is detached from the installer envelope.
            resolve_link(relative.parent().unwrap_or(Path::new("")), Path::new(link))?;
        }
        image.path = relative.to_str().ok_or(ArchiveError::Unsafe)?.into();
        files.push(image);
    }
    if files.is_empty() {
        return Err(ArchiveError::Unsafe);
    }
    Ok(ComponentImage { directory, files })
}

fn resolve_link(parent: &Path, link: &Path) -> Result<PathBuf, ArchiveError> {
    let mut path = parent.to_owned();
    for component in link.components() {
        match component {
            Component::Normal(value) => path.push(value),
            Component::CurDir => {}
            Component::ParentDir if path.pop() => {}
            _ => return Err(ArchiveError::Unsafe),
        }
    }
    if !safe_relative(&path) {
        return Err(ArchiveError::Unsafe);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_link_must_remain_inside_the_archive_root() {
        assert_eq!(
            resolve_link(Path::new("bin"), Path::new("../lib/npm.js")).unwrap(),
            Path::new("lib/npm.js")
        );
        assert!(resolve_link(Path::new("bin"), Path::new("../../outside")).is_err());
        assert!(resolve_link(Path::new("bin"), Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn link_chains_must_end_in_a_regular_file_without_cycles() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("python3.15");
        fs::write(&file, b"synthetic binary").unwrap();
        let first = root.path().join("python3");
        let second = root.path().join("python");
        let mut links = BTreeMap::from([(first.clone(), file), (second.clone(), first.clone())]);
        assert!(validate_link_chain(&second, &links).is_ok());
        links.insert(first, second.clone());
        assert!(validate_link_chain(&second, &links).is_err());
        assert!(validate_link_chain(root.path(), &BTreeMap::new()).is_err());
    }
}
