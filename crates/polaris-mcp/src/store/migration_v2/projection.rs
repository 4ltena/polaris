//! Streaming non-secret migration projection from already enumerated children.
//!
//! The root owner must enumerate every entry in relative-path byte order and
//! retain/recheck all ancestor descriptors. This reader does not establish a
//! complete inventory, stop proof, document schema, or activation admission.
//! It buffers one path and one read chunk, imposing no new whole-store limit.

use crate::{
    Error,
    entry_gate::{EntryAnchorV2, child_bytes},
    registration::valid_id,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, Metadata},
    io,
    os::unix::fs::{FileExt, MetadataExt},
};

pub(super) const MIGRATION_RECORD: &str = "migration.v2.json";
pub(super) const CATALOG_REGENERATION: &str = "catalog-regeneration.v2.json";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProjectionTreeV2 {
    SourceV1,
    ActivationV2,
}

pub(super) struct NonSecretTreeProjectionV2 {
    tree: ProjectionTreeV2,
    hash: Sha256,
    previous_path: Option<Vec<u8>>,
    poisoned: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Directory,
    PublicFile,
    SecretDocument,
    SourceLock,
    Epoch,
    PackageNode,
}

impl NonSecretTreeProjectionV2 {
    pub(super) fn new(tree: ProjectionTreeV2) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"polaris-mcp-non-secret-tree-v2\0");
        Self {
            tree,
            hash,
            previous_path: None,
            poisoned: false,
        }
    }

    /// Standalone reader used by focused projection fixtures. Production
    /// inventory supplies its exact opened and mount-validated descriptor.
    #[cfg(test)]
    pub(super) fn read_child(
        &mut self,
        relative_path: &[u8],
        parent: &File,
        name: &[u8],
    ) -> Result<(), Error> {
        let file = child_bytes(parent, name, libc::O_RDONLY | libc::O_NONBLOCK).map_err(|_| {
            self.poisoned = true;
            Error::Protection
        })?;
        self.read_opened_child(relative_path, parent, name, &file)
    }

    /// Hashes the exact descriptor held and mount-validated by inventory.
    /// The caller must validate its mount identity before and after this read.
    pub(super) fn read_opened_child(
        &mut self,
        relative_path: &[u8],
        parent: &File,
        name: &[u8],
        file: &File,
    ) -> Result<(), Error> {
        if self.poisoned {
            return Err(Error::Protection);
        }
        self.poisoned = true;
        if relative_path.split(|byte| *byte == b'/').next_back() != Some(name)
            || self
                .previous_path
                .as_deref()
                .is_some_and(|old| old >= relative_path)
        {
            return Err(Error::Protocol);
        }
        let mut role = classify(self.tree, relative_path)?;
        let parent_before = parent.metadata().map_err(storage)?;
        require_directory(&parent_before)?;
        let before = file.metadata().map_err(storage)?;
        if role == Role::PackageNode {
            role = if before.is_dir() {
                Role::Directory
            } else {
                Role::PublicFile
            };
        }
        match role {
            Role::Directory => require_directory(&before)?,
            _ => {
                require_regular(&before)?;
                if !relative_path.starts_with(b"servers/") && before.mode() & 0o7777 != 0o600 {
                    return Err(Error::Protection);
                }
                if role == Role::SourceLock && before.len() != 0 {
                    return Err(Error::Protection);
                }
            }
        }

        // Secret content and size are never fed to either hash. The epoch is
        // validated as a protected regular file but contributes no tuple.
        let file_hash = if matches!(role, Role::PublicFile | Role::SourceLock) {
            Some(read_hash(file, &before)?)
        } else {
            None
        };
        #[cfg(test)]
        AFTER_CHILD_READ.with(|hook| {
            if let Some(action) = hook.borrow_mut().take() {
                action();
            }
        });
        let after = file.metadata().map_err(storage)?;
        let named = child_bytes(parent, name, libc::O_RDONLY | libc::O_NONBLOCK)
            .map_err(|_| Error::Protection)?;
        let parent_after = parent.metadata().map_err(storage)?;
        if !stable(&before, &after)
            || !stable(&before, &named.metadata().map_err(storage)?)
            || !stable(&parent_before, &parent_after)
        {
            return Err(Error::Protection);
        }
        if role != Role::Epoch {
            self.hash.update([match role {
                Role::Directory => b'd',
                Role::SecretDocument => b's',
                _ => b'f',
            }]);
            self.hash.update((relative_path.len() as u64).to_be_bytes());
            self.hash.update(relative_path);
            self.hash.update((before.mode() & 0o7777).to_be_bytes());
            self.hash.update(before.uid().to_be_bytes());
            if let Some(hash) = file_hash {
                self.hash.update(before.len().to_be_bytes());
                self.hash.update(hash);
            }
        }
        self.previous_path = Some(relative_path.to_owned());
        self.poisoned = false;
        Ok(())
    }

    pub(super) fn finish(self) -> Result<String, Error> {
        if self.poisoned {
            return Err(Error::Protection);
        }
        Ok(format!("{:x}", self.hash.finalize()))
    }
}

fn classify(tree: ProjectionTreeV2, path: &[u8]) -> Result<Role, Error> {
    let components: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
    if components
        .iter()
        .any(|part| part.is_empty() || *part == b"." || *part == b".." || part.contains(&0))
    {
        return Err(Error::Protocol);
    }
    if components[0] == b"servers" {
        if components.len() == 1 {
            return Ok(Role::Directory);
        }
        let id = std::str::from_utf8(components[1]).map_err(|_| Error::Protocol)?;
        if !valid_id(id) {
            return Err(Error::Protocol);
        }
        return Ok(if components.len() == 2 {
            Role::Directory
        } else {
            Role::PackageNode
        });
    }
    if components.len() == 1 {
        return match components[0] {
            b"servers.json" | b"catalog.json" => Ok(Role::PublicFile),
            b"secrets.json" => Ok(Role::SecretDocument),
            b"owner.lock" if tree == ProjectionTreeV2::SourceV1 => Ok(Role::SourceLock),
            b"ledger" if tree == ProjectionTreeV2::ActivationV2 => Ok(Role::Directory),
            value
                if tree == ProjectionTreeV2::ActivationV2
                    && (value == MIGRATION_RECORD.as_bytes()
                        || value == CATALOG_REGENERATION.as_bytes()) =>
            {
                Ok(Role::PublicFile)
            }
            _ => Err(Error::Protocol),
        };
    }
    if tree == ProjectionTreeV2::ActivationV2 && components.len() == 2 && components[0] == b"ledger"
    {
        return match components[1] {
            b"head.json" | b"history.jsonl" => Ok(Role::PublicFile),
            b"epoch.json" => Ok(Role::Epoch),
            _ => Err(Error::Protocol),
        };
    }
    Err(Error::Protocol)
}

fn require_directory(meta: &Metadata) -> Result<(), Error> {
    if !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7022 != 0
        || meta.nlink() == 0
    {
        Err(Error::Protection)
    } else {
        Ok(())
    }
}
fn require_regular(meta: &Metadata) -> Result<(), Error> {
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7022 != 0
        || meta.nlink() != 1
    {
        Err(Error::Protection)
    } else {
        Ok(())
    }
}

fn read_hash(file: &File, before: &Metadata) -> Result<[u8; 32], Error> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 32768];
    let mut offset = 0u64;
    while offset < before.len() {
        let limit = usize::try_from((before.len() - offset).min(buffer.len() as u64))
            .map_err(|_| Error::Limit)?;
        let count = match file.read_at(&mut buffer[..limit], offset) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result.map_err(storage)?,
        };
        if count == 0 {
            return Err(Error::Protection);
        }
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    loop {
        match file.read_at(&mut buffer[..1], offset) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(0) => break,
            Ok(_) => return Err(Error::Protection),
            Err(error) => return Err(storage(error)),
        }
    }
    Ok(hash.finalize().into())
}

fn stable(a: &Metadata, b: &Metadata) -> bool {
    EntryAnchorV2::of(a) == EntryAnchorV2::of(b)
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn storage(_: io::Error) -> Error {
    Error::Storage
}

#[cfg(test)]
thread_local! {
    static AFTER_CHILD_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
