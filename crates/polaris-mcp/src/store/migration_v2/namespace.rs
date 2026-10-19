//! Retained descriptor and namespace observations, never an entry/stop grant.
use crate::{Error, NativeMountIdentityV2};
use polaris_tools::entry_kernel::{EntryAnchorV2, child, owned_directory, physical_directory};
use std::{
    fs::File,
    path::{Path, PathBuf},
};

/// A read-only physical home/parent binding. The migration owner must separately
/// verify its OS account-home capability and still-held exclusive entry guard.
/// Neither the logical path nor the observed anchor authorizes a mutation.
pub struct MigrationParentObservationV2 {
    home_path: PathBuf,
    home: File,
    home_anchor: EntryAnchorV2,
    parent: File,
    parent_anchor: EntryAnchorV2,
    mount: NativeMountIdentityV2,
}
impl MigrationParentObservationV2 {
    pub fn inspect(home: &Path, parent: &File) -> Result<Self, Error> {
        let home_fd = physical_directory(home).map_err(|_| Error::Protection)?;
        owned_directory(&home_fd, false).map_err(|_| Error::Protection)?;
        owned_directory(parent, false).map_err(|_| Error::Protection)?;
        let value = Self {
            home_path: home.into(),
            home_anchor: anchor(&home_fd)?,
            home: home_fd,
            parent: parent.try_clone().map_err(|_| Error::Storage)?,
            parent_anchor: anchor(parent)?,
            mount: NativeMountIdentityV2::inspect(parent)?,
        };
        value.verify_current()?;
        Ok(value)
    }
    pub fn verify_current(&self) -> Result<(), Error> {
        for _ in 0..2 {
            let named_home = physical_directory(&self.home_path).map_err(|_| Error::Protection)?;
            for home in [&self.home, &named_home] {
                owned_directory(home, false).map_err(|_| Error::Protection)?;
                if !same_parent(&self.home_anchor, &anchor(home)?)
                    || NativeMountIdentityV2::inspect(home)? != self.mount
                {
                    return Err(Error::Protection);
                }
            }
            let named_parent = child(&self.home, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY)
                .map_err(|_| Error::Protection)?;
            for parent in [&self.parent, &named_parent] {
                owned_directory(parent, false).map_err(|_| Error::Protection)?;
                if !same_parent(&self.parent_anchor, &anchor(parent)?)
                    || NativeMountIdentityV2::inspect(parent)? != self.mount
                {
                    return Err(Error::Protection);
                }
            }
        }
        Ok(())
    }
    pub fn logical_home(&self) -> &Path {
        &self.home_path
    }
    pub(crate) fn parent(&self) -> &File {
        &self.parent
    }
}
pub(super) fn same_parent(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    (a.device, a.inode, a.uid, a.mode) == (b.device, b.inode, b.uid, b.mode)
}
fn anchor(file: &File) -> Result<EntryAnchorV2, Error> {
    Ok(EntryAnchorV2::of(
        &file.metadata().map_err(|_| Error::Storage)?,
    ))
}
