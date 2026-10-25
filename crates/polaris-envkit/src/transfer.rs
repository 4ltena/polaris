//! Plan-scoped, credential-free host transfers. The effect owner must persist
//! Started before calling this module and keep its lease until completion.
use crate::{Artifact, Error};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error(transparent)]
    Plan(#[from] Error),
    #[error("environment artifact cache is unavailable or unsafe")]
    Cache,
    #[error("environment artifact does not match its approved digest")]
    Digest,
    #[error(transparent)]
    Transport(#[from] polaris_http::artifact::Error),
}

pub(crate) async fn fetch_one(
    artifact: &Artifact,
    redirects: &[String],
    cache: &Path,
    mut cancelled: impl FnMut() -> bool + Send,
) -> Result<PathBuf, TransferError> {
    if cancelled() {
        return Err(polaris_http::artifact::Error::Cancelled.into());
    }
    let destination = cache.join(format!("{}.tgz", artifact.sha256));
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_cached(&destination, artifact)?;
            return Ok(destination);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(TransferError::Cache),
    }
    let source =
        polaris_http::artifact::ArtifactSource::new(&artifact.url, redirects, artifact.bytes)?;
    let mut temporary = tempfile::NamedTempFile::new_in(cache).map_err(|_| TransferError::Cache)?;
    let mut hash = Sha256::new();
    polaris_http::artifact::fetch_verified(
        &source,
        |chunk| {
            temporary.write_all(chunk).map_err(|_| ())?;
            hash.update(chunk);
            Ok(())
        },
        &mut cancelled,
    )
    .await?;
    if format!("{:x}", hash.finalize()) != artifact.sha256 {
        return Err(TransferError::Digest);
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| TransferError::Cache)?;
    temporary
        .persist_noclobber(&destination)
        .map_err(|_| TransferError::Cache)?;
    File::open(cache)
        .and_then(|file| file.sync_all())
        .map_err(|_| TransferError::Cache)?;
    verify_cached(&destination, artifact)?;
    Ok(destination)
}

fn verify_cached(path: &Path, artifact: &Artifact) -> Result<(), TransferError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| TransferError::Cache)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != artifact.bytes
    {
        return Err(TransferError::Cache);
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|_| TransferError::Cache)?;
    let opened = file.metadata().map_err(|_| TransferError::Cache)?;
    if !opened.is_file() || opened.len() != artifact.bytes {
        return Err(TransferError::Cache);
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let count = file.read(&mut buffer).map_err(|_| TransferError::Cache)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(TransferError::Digest)?;
        if bytes > artifact.bytes {
            return Err(TransferError::Digest);
        }
        hash.update(&buffer[..count]);
    }
    if bytes != artifact.bytes || format!("{:x}", hash.finalize()) != artifact.sha256 {
        return Err(TransferError::Digest);
    }
    Ok(())
}
