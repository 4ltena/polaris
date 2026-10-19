//! Complete descriptor-pinned inventory for the migration's non-secret projection.
//!
//! The caller supplies a current-UID, protected, physically opened root FD and
//! binds that FD to the canonical parent/name before and after each operation.
//! This owner cannot prove that external binding or production-process stopping.
//! A legitimate root rename can change its timestamps: the migration owner must
//! inspect the retired FD anew, then compare its immutable anchor, projection and
//! documents with the pre-retire inventory. There is no arbitrary rebase API.
//!
//! Directory descriptors and exact child sets remain pinned. Every opened node
//! must remain on the root mount; repeated directory device/inode is rejected
//! before descent, including aliases and cycles back to the root. Public files
//! are streamed by the projection reader; secret bytes are never read here. No new
//! store-wide entry limit or registry-count limit is imposed.

use super::projection::{NonSecretTreeProjectionV2, ProjectionTreeV2};
use crate::native_mount::{NativeMountIdentityV2 as MountIdentityV2, platform_mount_identity};

use crate::{
    Error,
    entry_gate::{EntryAnchorV2, child_bytes},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CStr,
    fs::{File, Metadata},
    io,
    os::{fd::IntoRawFd, unix::fs::MetadataExt},
};

pub struct CompleteTreeProjectionV2 {
    tree: ProjectionTreeV2,
    directories: Vec<DirectoryPin>,
    entries: Vec<EntryPin>,
    digest: String,
    mount: MountIdentityV2,
    package_protection: Option<PackageProtectionV2>,
}

// Captured only by the source preflight constructor. Root store documents are
// interpreted by their closed codecs; package payloads must never contain a
// registered/prohibited secret path or a current/retired protected identity.
struct PackageProtectionV2 {
    root: std::path::PathBuf,
    paths: BTreeSet<std::path::PathBuf>,
    identities: BTreeSet<polaris_auth::protection::ProtectedIdentity>,
}
impl PackageProtectionV2 {
    fn inspect(root: &std::path::Path) -> Result<Self, Error> {
        let normalized: std::path::PathBuf = root.components().collect();
        if !root.is_absolute() || normalized.as_os_str() != root.as_os_str() {
            return Err(Error::Protection);
        }
        let (paths, identities) = polaris_auth::protection::with_protected_paths(|snapshot| {
            (snapshot.paths().clone(), snapshot.identities().clone())
        })
        .map_err(|_| Error::Protection)?;
        Ok(Self {
            root: root.into(),
            paths,
            identities,
        })
    }
    fn refreshed(&self) -> Result<Self, Error> {
        let mut fresh = Self::inspect(&self.root)?;
        fresh.paths.extend(self.paths.iter().cloned());
        fresh.identities.extend(self.identities.iter().copied());
        Ok(fresh)
    }
    fn check(&self, relative: &[u8], metadata: &Metadata) -> Result<(), Error> {
        use std::os::unix::ffi::OsStrExt;
        if relative != b"servers" && !relative.starts_with(b"servers/") {
            return Ok(());
        }
        let path = self.root.join(std::ffi::OsStr::from_bytes(relative));
        if polaris_tools::path_policy::is_denied(&path)
            || self
                .paths
                .iter()
                .any(|p| path.starts_with(p) || p.starts_with(&path))
            || self
                .identities
                .contains(&polaris_auth::protection::ProtectedIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                })
        {
            return Err(Error::Protection);
        }
        Ok(())
    }
}

struct DirectoryPin {
    file: File,
    metadata: Metadata,
    parent: Option<(usize, Vec<u8>)>,
    relative_path: Vec<u8>,
    names: BTreeSet<Vec<u8>>,
}

struct EntryPin {
    parent: usize,
    name: Vec<u8>,
    relative_path: Vec<u8>,
    metadata: Metadata,
    directory: bool,
}

struct PendingChild {
    parent: usize,
    name: Vec<u8>,
}

/// Borrowed read capability returned only after the complete inventory closes.
/// Copying remains a separate owner operation and must close with another
/// verification before it accepts the copied bytes or publishes a destination.
pub struct VerifiedTreeCopyV2<'a> {
    inventory: &'a CompleteTreeProjectionV2,
}

pub struct VerifiedTreeEntryV2<'a> {
    inventory: &'a CompleteTreeProjectionV2,
    entry: &'a EntryPin,
}

impl CompleteTreeProjectionV2 {
    pub fn inspect(root: &File, tree: ProjectionTreeV2) -> Result<Self, Error> {
        Self::inspect_inner(root, tree, None)
    }

    /// Used only with the preflight owner's physically bound source path. The
    /// protection check happens before any package file enters a public hash.
    pub fn inspect_source(root: &File, path: &std::path::Path) -> Result<Self, Error> {
        Self::inspect_inner(
            root,
            ProjectionTreeV2::SourceV1,
            Some(PackageProtectionV2::inspect(path)?),
        )
    }

    fn inspect_inner(
        root: &File,
        tree: ProjectionTreeV2,
        package_protection: Option<PackageProtectionV2>,
    ) -> Result<Self, Error> {
        let root = root.try_clone().map_err(storage)?;
        let metadata = root.metadata().map_err(storage)?;
        require_directory(&metadata)?;
        let mount = mount_identity(&root)?;
        let mut seen_directories = BTreeSet::from([directory_identity(&metadata)]);
        let names = enumerate(&root, b"", &mount)?;
        if !stable(&metadata, &root.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        let mut inventory = Self {
            tree,
            directories: vec![DirectoryPin {
                file: root,
                metadata,
                parent: None,
                relative_path: Vec::new(),
                names,
            }],
            entries: Vec::new(),
            digest: String::new(),
            mount,
            package_protection,
        };
        let mut pending = BTreeMap::new();
        inventory.queue_children(0, &mut pending)?;
        let mut projection = NonSecretTreeProjectionV2::new(tree);

        // The frontier orders entire raw relative paths. Descending immediately
        // into "foo" would wrongly put "foo/child" before sibling "foo-bar".
        while let Some((relative_path, child)) = pending.pop_first() {
            inventory.verify_directory(child.parent, false)?;
            let parent = &inventory.directories[child.parent].file;
            let file = open_child(parent, &child.name)?;
            #[cfg(test)]
            projection_read_point(ProjectionReadPointV2::Opened, &file);
            inventory.require_mount(&file)?;
            let metadata = file.metadata().map_err(storage)?;
            require_node(&metadata)?;
            if let Some(protection) = &inventory.package_protection {
                protection.check(&relative_path, &metadata)?;
            }
            if metadata.is_dir() && !seen_directories.insert(directory_identity(&metadata)) {
                // Even a same-mount directory alias cannot reappear. This
                // closes both sibling aliases and a cycle back to the root.
                return Err(Error::Protection);
            }
            #[cfg(test)]
            projection_read_point(ProjectionReadPointV2::BeforeHash, &file);
            projection.read_opened_child(&relative_path, parent, &child.name, &file)?;
            inventory.require_mount(&file)?;
            let named = open_child(parent, &child.name)?;
            inventory.require_mount(&named)?;
            if !stable(&metadata, &file.metadata().map_err(storage)?)
                || !stable(&metadata, &named.metadata().map_err(storage)?)
            {
                return Err(Error::Protection);
            }
            let directory = metadata.is_dir();
            if directory {
                let names = enumerate(&file, &relative_path, &inventory.mount)?;
                if !stable(&metadata, &file.metadata().map_err(storage)?) {
                    return Err(Error::Protection);
                }
                let directory_index = inventory.directories.len();
                inventory.directories.push(DirectoryPin {
                    file,
                    metadata: metadata.clone(),
                    parent: Some((child.parent, child.name.clone())),
                    relative_path: relative_path.clone(),
                    names,
                });
                inventory.queue_children(directory_index, &mut pending)?;
            }
            inventory.entries.push(EntryPin {
                parent: child.parent,
                name: child.name,
                relative_path,
                metadata,
                directory,
            });
        }

        // This also seals empty directories and directories whose last readdir
        // precedes the final public-file read.
        inventory.verify_all_metadata_and_sets()?;
        inventory.digest = projection.finish()?;
        Ok(inventory)
    }

    /// Verified digest only; raw names, file sizes and secret-derived metadata
    /// cannot be serialized or formatted through this capability.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// A source projection cannot authorize reading an activation payload under
    /// its different file-role rules, even when both pin the same directory.
    pub fn require_activation_tree(&self) -> Result<(), Error> {
        if self.tree != ProjectionTreeV2::ActivationV2 {
            return Err(Error::Protocol);
        }
        Ok(())
    }

    /// Checks the held inventory root and another descriptor against the same
    /// captured native mount. This does not validate the other object's inode,
    /// ownership, namespace binding, or absence of aliases.
    pub fn verify_same_mount(&self, other: &File) -> Result<(), Error> {
        let held_root = &self.directories[0].file;
        self.require_mount(held_root)?;
        self.require_mount(other)?;
        self.require_mount(held_root)?;
        self.require_mount(other)
    }

    /// The migration owner supplies a fresh nofollow descriptor opened through
    /// its pinned external parent/name. This checks root/mount identity only;
    /// it neither proves that provenance nor rebases this inventory's metadata.
    /// An authorized rename may change timestamps, so the retired tree still
    /// needs a fresh inspect and the complete document/projection comparison.
    pub fn verify_root_mount_binding(&self, named_root: &File) -> Result<(), Error> {
        let held = &self.directories[0];
        self.require_mount(&held.file)?;
        self.require_mount(named_root)?;
        let held_metadata = held.file.metadata().map_err(storage)?;
        let named_metadata = named_root.metadata().map_err(storage)?;
        require_directory(&held_metadata)?;
        require_directory(&named_metadata)?;
        if EntryAnchorV2::of(&held_metadata) != EntryAnchorV2::of(&held.metadata)
            || EntryAnchorV2::of(&named_metadata) != EntryAnchorV2::of(&held.metadata)
        {
            return Err(Error::Protection);
        }
        self.require_mount(&held.file)?;
        self.require_mount(named_root)
    }

    pub fn verify_current(&self) -> Result<(), Error> {
        self.verify_projection()
    }

    pub fn verify_before_retire(&self) -> Result<(), Error> {
        self.verify_current()
    }

    pub fn verify_before_copy(&self) -> Result<VerifiedTreeCopyV2<'_>, Error> {
        self.verify_projection()?;
        Ok(VerifiedTreeCopyV2 { inventory: self })
    }

    /// Reobserve a host-retired source through the same root FD. Only root
    /// ctime may change; all child identities/full stamps and sets stay pinned.
    /// This supplies no rename or activation authority.
    pub fn inspect_retired_same_root(&self, root: &File) -> Result<Self, Error> {
        let protection = self.package_protection.as_ref().ok_or(Error::Protection)?;
        if self.tree != ProjectionTreeV2::SourceV1 {
            return Err(Error::Protection);
        }
        let verify = || -> Result<(), Error> {
            self.verify_root_mount_binding(root)?;
            let pin = &self.directories[0];
            let old = &pin.metadata;
            let current = pin.file.metadata().map_err(storage)?;
            if current.len() != old.len()
                || (current.mtime(), current.mtime_nsec()) != (old.mtime(), old.mtime_nsec())
                || enumerate(&pin.file, &pin.relative_path, &self.mount)? != pin.names
            {
                return Err(Error::Protection);
            }
            for directory in 1..self.directories.len() {
                self.verify_directory(directory, true)?;
            }
            for entry in &self.entries {
                self.verify_entry(entry)?;
            }
            for directory in 1..self.directories.len() {
                self.verify_directory(directory, false)?;
            }
            self.verify_root_mount_binding(root)
        };
        verify()?;
        // Preserve the original logical source path and any protection pins
        // captured earlier, including identities subsequently deregistered.
        let fresh = Self::inspect_inner(root, self.tree, Some(protection.refreshed()?))?;
        if fresh.digest != self.digest {
            return Err(Error::Protection);
        }
        verify()?;
        fresh.verify_current()?;
        Ok(fresh)
    }

    fn queue_children(
        &self,
        directory: usize,
        pending: &mut BTreeMap<Vec<u8>, PendingChild>,
    ) -> Result<(), Error> {
        let pin = &self.directories[directory];
        for name in &pin.names {
            let mut relative_path = pin.relative_path.clone();
            if !relative_path.is_empty() {
                relative_path.push(b'/');
            }
            relative_path.extend_from_slice(name);
            if pending
                .insert(
                    relative_path,
                    PendingChild {
                        parent: directory,
                        name: name.clone(),
                    },
                )
                .is_some()
            {
                return Err(Error::Protection);
            }
        }
        Ok(())
    }

    fn require_mount(&self, file: &File) -> Result<(), Error> {
        require_mount(file, &self.mount)
    }

    fn verify_projection(&self) -> Result<(), Error> {
        self.verify_all_metadata_and_sets()?;
        let mut projection = NonSecretTreeProjectionV2::new(self.tree);
        let protection = self
            .package_protection
            .as_ref()
            .map(PackageProtectionV2::refreshed)
            .transpose()?;
        for entry in &self.entries {
            self.verify_path_ancestry(entry.parent)?;
            let parent = &self.directories[entry.parent].file;
            let file = open_child(parent, &entry.name)?;
            #[cfg(test)]
            projection_read_point(ProjectionReadPointV2::Opened, &file);
            self.require_mount(&file)?;
            let before = file.metadata().map_err(storage)?;
            require_node(&before)?;
            if let Some(protection) = &protection {
                protection.check(&entry.relative_path, &before)?;
            }
            if !stable(&entry.metadata, &before) {
                return Err(Error::Protection);
            }
            // The projection must hash the FD whose mount and inventoried
            // metadata were checked above, rather than reopening another FD.
            #[cfg(test)]
            projection_read_point(ProjectionReadPointV2::BeforeHash, &file);
            projection.read_opened_child(&entry.relative_path, parent, &entry.name, &file)?;
            self.require_mount(&file)?;
            let named = open_child(parent, &entry.name)?;
            self.require_mount(&named)?;
            let held_after = file.metadata().map_err(storage)?;
            let named_after = named.metadata().map_err(storage)?;
            require_node(&held_after)?;
            require_node(&named_after)?;
            if !stable(&entry.metadata, &held_after) || !stable(&entry.metadata, &named_after) {
                return Err(Error::Protection);
            }
            self.verify_path_ancestry(entry.parent)?;
        }
        if projection.finish()? != self.digest {
            return Err(Error::Protection);
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_PROJECTION.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        // A file or named ancestor can change after its projection was read.
        // Recheck every node plus the last exact child-set enumeration at EOF.
        self.verify_all_metadata_and_sets()
    }

    fn verify_all_metadata_and_sets(&self) -> Result<(), Error> {
        for directory in 0..self.directories.len() {
            self.verify_directory(directory, true)?;
        }
        for entry in &self.entries {
            self.verify_entry(entry)?;
        }
        // Closing named/held checks cover a directory replaced while a later
        // directory or file was being inspected.
        self.verify_all_ancestry()
    }

    fn verify_all_ancestry(&self) -> Result<(), Error> {
        for directory in 0..self.directories.len() {
            self.verify_directory(directory, false)?;
        }
        Ok(())
    }

    fn verify_path_ancestry(&self, mut directory: usize) -> Result<(), Error> {
        loop {
            self.verify_directory(directory, false)?;
            match &self.directories[directory].parent {
                Some((parent, _)) => directory = *parent,
                None => return Ok(()),
            }
        }
    }

    fn verify_directory(&self, index: usize, children: bool) -> Result<(), Error> {
        let pin = &self.directories[index];
        self.require_mount(&pin.file)?;
        let held = pin.file.metadata().map_err(storage)?;
        require_directory(&held)?;
        if !stable(&pin.metadata, &held) {
            return Err(Error::Protection);
        }
        let named = match &pin.parent {
            Some((parent, name)) => open_child(&self.directories[*parent].file, name)?,
            None => child_bytes(&pin.file, b".", libc::O_RDONLY | libc::O_DIRECTORY)
                .map_err(open_error)?,
        };
        self.require_mount(&named)?;
        let named_metadata = named.metadata().map_err(storage)?;
        require_directory(&named_metadata)?;
        if !stable(&pin.metadata, &named_metadata) {
            return Err(Error::Protection);
        }
        if children && enumerate(&pin.file, &pin.relative_path, &self.mount)? != pin.names {
            return Err(Error::Protection);
        }
        self.require_mount(&pin.file)?;
        self.require_mount(&named)?;
        if !stable(&pin.metadata, &pin.file.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        Ok(())
    }

    fn verify_entry(&self, entry: &EntryPin) -> Result<(), Error> {
        let file = open_child(&self.directories[entry.parent].file, &entry.name)?;
        self.require_mount(&file)?;
        let metadata = file.metadata().map_err(storage)?;
        require_node(&metadata)?;
        if !stable(&entry.metadata, &metadata) {
            return Err(Error::Protection);
        }
        Ok(())
    }
}

impl<'a> VerifiedTreeCopyV2<'a> {
    pub fn entries(&self) -> impl Iterator<Item = VerifiedTreeEntryV2<'a>> {
        let inventory = self.inventory;
        inventory
            .entries
            .iter()
            .map(move |entry| VerifiedTreeEntryV2 { inventory, entry })
    }

    pub fn verify_after_copy(self) -> Result<(), Error> {
        self.inventory.verify_projection()
    }
}

impl VerifiedTreeEntryV2<'_> {
    pub fn relative_path(&self) -> &[u8] {
        &self.entry.relative_path
    }

    pub fn is_directory(&self) -> bool {
        self.entry.directory
    }

    pub fn anchor(&self) -> EntryAnchorV2 {
        EntryAnchorV2::of(&self.entry.metadata)
    }

    /// A nofollow descriptor for the exact inventoried object. The copy owner
    /// must retain it while streaming and require verify_after_copy afterward.
    /// This method itself does not read secret or public content.
    pub fn open_readonly(&self) -> Result<File, Error> {
        self.inventory.verify_path_ancestry(self.entry.parent)?;
        let file = open_child(
            &self.inventory.directories[self.entry.parent].file,
            &self.entry.name,
        )?;
        self.inventory.require_mount(&file)?;
        let metadata = file.metadata().map_err(storage)?;
        require_node(&metadata)?;
        if !stable(&self.entry.metadata, &metadata) {
            return Err(Error::Protection);
        }
        self.inventory.verify_path_ancestry(self.entry.parent)?;
        self.inventory.require_mount(&file)?;
        if !stable(&self.entry.metadata, &file.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        Ok(file)
    }
}

fn directory_identity(metadata: &Metadata) -> (u64, u64) {
    let identity = (metadata.dev(), metadata.ino());
    #[cfg(test)]
    if let Some(hook) = DIRECTORY_IDENTITY_OVERRIDE
        .with(|slot| slot.borrow_mut().as_mut().map(|hook| hook(identity)))
    {
        return hook;
    }
    identity
}

fn require_mount(file: &File, expected: &MountIdentityV2) -> Result<(), Error> {
    if mount_identity(file)? != *expected {
        return Err(Error::Protection);
    }
    Ok(())
}

fn mount_identity(file: &File) -> Result<MountIdentityV2, Error> {
    let identity = platform_mount_identity(file)?;
    #[cfg(test)]
    {
        return MOUNT_IDENTITY_OVERRIDE.with(|slot| match slot.borrow_mut().as_mut() {
            Some(hook) => hook(file, identity),
            None => Ok(identity),
        });
    }
    #[cfg(not(test))]
    Ok(identity)
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&0) && !name.contains(&b'/')
}

fn open_child(parent: &File, name: &[u8]) -> Result<File, Error> {
    if !valid_name(name) {
        return Err(Error::Protection);
    }
    child_bytes(parent, name, libc::O_RDONLY | libc::O_NONBLOCK).map_err(open_error)
}

fn require_directory(metadata: &Metadata) -> Result<(), Error> {
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7022 != 0
        || metadata.nlink() == 0
    {
        return Err(Error::Protection);
    }
    Ok(())
}

fn require_node(metadata: &Metadata) -> Result<(), Error> {
    if metadata.is_dir() {
        return require_directory(metadata);
    }
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7022 != 0
        || metadata.nlink() != 1
    {
        return Err(Error::Protection);
    }
    Ok(())
}

fn stable(a: &Metadata, b: &Metadata) -> bool {
    EntryAnchorV2::of(a) == EntryAnchorV2::of(b)
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn enumerate(
    directory: &File,
    relative_path: &[u8],
    mount: &MountIdentityV2,
) -> Result<BTreeSet<Vec<u8>>, Error> {
    require_mount(directory, mount)?;
    let before = directory.metadata().map_err(storage)?;
    require_directory(&before)?;
    // A fresh openat owns an independent directory cursor; dup shares offsets.
    let independent =
        child_bytes(directory, b".", libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
    require_mount(&independent, mount)?;
    if !stable(&before, &independent.metadata().map_err(storage)?) {
        return Err(Error::Protection);
    }
    let fd = independent.into_raw_fd();
    let pointer = unsafe { libc::fdopendir(fd) };
    if pointer.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(Error::Storage);
    }
    let stream = DirectoryStream(pointer);
    let mut names = BTreeSet::new();
    loop {
        clear_errno();
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if errno() != 0 {
                return Err(Error::Storage);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if !valid_name(name) || !names.insert(name.to_owned()) {
            return Err(Error::Protection);
        }
    }
    #[cfg(test)]
    AFTER_ENUMERATION_EOF.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(hook) = slot.as_mut() {
            if hook(relative_path) {
                *slot = None;
            }
        }
    });
    #[cfg(not(test))]
    let _ = relative_path;
    let closing =
        child_bytes(directory, b".", libc::O_RDONLY | libc::O_DIRECTORY).map_err(open_error)?;
    require_mount(directory, mount)?;
    require_mount(&closing, mount)?;
    let after = directory.metadata().map_err(storage)?;
    if !stable(&before, &after) || !stable(&before, &closing.metadata().map_err(storage)?) {
        return Err(Error::Protection);
    }
    Ok(names)
}

#[cfg(target_os = "macos")]
fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__error() }
}
#[cfg(target_os = "linux")]
fn errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}
fn clear_errno() {
    unsafe {
        *errno_pointer() = 0;
    }
}
fn errno() -> libc::c_int {
    unsafe { *errno_pointer() }
}
fn open_error(_: io::Error) -> Error {
    Error::Protection
}
fn storage(_: io::Error) -> Error {
    Error::Storage
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectionReadPointV2 {
    Opened,
    BeforeHash,
}

#[cfg(test)]
fn projection_read_point(point: ProjectionReadPointV2, file: &File) {
    PROJECTION_READ_POINT.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(point, file);
        }
    });
}

#[cfg(test)]
thread_local! {
    // true consumes the hook. Tests can target a particular directory's final
    // enumeration rather than accidentally injecting into an earlier pass.
    static AFTER_ENUMERATION_EOF:
        std::cell::RefCell<Option<Box<dyn FnMut(&[u8]) -> bool>>> =
        const { std::cell::RefCell::new(None) };
    static AFTER_PROJECTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    static MOUNT_IDENTITY_OVERRIDE: std::cell::RefCell<Option<Box<dyn FnMut(&File, MountIdentityV2) -> Result<MountIdentityV2, Error>>>> =
        const { std::cell::RefCell::new(None) };
    static DIRECTORY_IDENTITY_OVERRIDE: std::cell::RefCell<Option<Box<dyn FnMut((u64, u64)) -> (u64, u64)>>> =
        const { std::cell::RefCell::new(None) };
    static PROJECTION_READ_POINT: std::cell::RefCell<Option<Box<dyn FnMut(ProjectionReadPointV2, &File)>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
