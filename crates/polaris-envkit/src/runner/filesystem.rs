//! Owner-local filesystem materialization; admission and cleanup stay in runner.
use super::*;

#[cfg(unix)]
pub(super) fn private_directory(path: &Path) -> Result<(), RunError> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| RunError::State)?;
    File::open(path.parent().ok_or(RunError::State)?)
        .and_then(|file| file.sync_all())
        .map_err(|_| RunError::State)
}

#[cfg(unix)]
pub(super) fn create_file(path: &Path, bytes: &[u8]) -> Result<(), RunError> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().ok_or(RunError::State)?;
    // All parents are inside the owner-created private staging root. Source
    // admission excludes symlinks and no child is running while materializing.
    fs::create_dir_all(parent).map_err(|_| RunError::State)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map_err(|_| RunError::State)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| RunError::State)?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|_| RunError::State)
}

#[cfg(unix)]
pub(super) fn merge_runtime(
    source: &Path,
    destination: &Path,
    installer_metadata: bool,
) -> Result<(), RunError> {
    use std::os::unix::fs::{DirBuilderExt, symlink};
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(destination)
                .map_err(|_| RunError::State)?;
        }
        _ => return Err(RunError::State),
    }
    for entry in fs::read_dir(source).map_err(|_| RunError::State)? {
        let entry = entry.map_err(|_| RunError::State)?;
        if installer_metadata && entry.file_name() == "manifest.in" {
            continue;
        }
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from).map_err(|_| RunError::State)?;
        if metadata.is_dir() {
            merge_runtime(&from, &to, false)?;
        } else {
            if fs::symlink_metadata(&to).is_ok() {
                return Err(RunError::State);
            }
            if metadata.file_type().is_symlink() {
                symlink(fs::read_link(&from).map_err(|_| RunError::State)?, &to)
                    .map_err(|_| RunError::State)?;
            } else if metadata.is_file() {
                fs::rename(&from, &to).map_err(|_| RunError::State)?;
            } else {
                return Err(RunError::State);
            }
        }
    }
    File::open(destination)
        .and_then(|file| file.sync_all())
        .map_err(|_| RunError::State)
}

#[cfg(unix)]
pub(super) fn relocate_venv(stage: &Path, target: &Path) -> Result<(), RunError> {
    use std::os::unix::fs::OpenOptionsExt;
    let old = stage.to_str().ok_or(RunError::State)?;
    let new = target.to_str().ok_or(RunError::State)?;
    let mut paths = vec![stage.join(".venv/pyvenv.cfg")];
    for entry in fs::read_dir(stage.join(".venv/bin")).map_err(|_| RunError::State)? {
        let entry = entry.map_err(|_| RunError::State)?;
        if entry.metadata().map_err(|_| RunError::State)?.len() <= 1024 * 1024 {
            paths.push(entry.path());
        }
    }
    for path in paths {
        let bytes = fs::read(&path).map_err(|_| RunError::State)?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if !text.contains(old) {
            continue;
        }
        let updated = text.replace(old, new);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| RunError::State)?;
        file.write_all(updated.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| RunError::State)?;
    }
    Ok(())
}
