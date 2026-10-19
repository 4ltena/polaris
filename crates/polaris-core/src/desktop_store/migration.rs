//! Simultaneously retained observations of the supplied transport roots.
//! Root discovery and native stopping belong to the private MCP coordinator.
use super::{
    StoreError, StoreResult, disk,
    session_manager::family::migration::FamilyTransportInspectionV2,
    writer::migration::{
        local::LocalSessionsTransportInspectionV2, native_mount,
        root::DesktopRootTransportInspectionV2,
    },
};
use polaris_tools::entry_kernel::{EntryAnchorV2, child, owned_directory};
use std::{
    collections::BTreeSet,
    fs::File,
    path::{Component, Path, PathBuf},
};

const MAX_ROOTS: usize = 65_536;
pub(crate) mod discovery;
mod legacy;
mod management;

pub(crate) enum TransportRootV2 {
    LocalData(PathBuf),
    ManagerFamilies(PathBuf),
    Desktop(PathBuf),
    ManagementProject(PathBuf),
}
impl TransportRootV2 {
    fn path(&self) -> &Path {
        match self {
            Self::LocalData(p)
            | Self::ManagerFamilies(p)
            | Self::Desktop(p)
            | Self::ManagementProject(p) => p,
        }
    }
}

pub(crate) struct TransportRootsObservationV2 {
    roots: Vec<RootObservation>,
}
struct RootObservation {
    path: PathPin,
    contents: Option<Contents>,
}
enum Contents {
    Local {
        current: LocalSessionsTransportInspectionV2,
        legacy: legacy::LegacySessionsObservation,
    },
    Families(FamilyTransportInspectionV2),
    Desktop(DesktopRootTransportInspectionV2),
    Management(Box<management::ManagementProjectObservationV2>),
}
enum ManagementUpdateV2 {
    Renamed(crate::management_effect::ManagementEffectHashV1),
    Tombstone,
    Stage(File),
    Finished(Box<crate::management_effect::ManagementMigrationTreeV2>),
}
impl Contents {
    fn verify_current(&self) -> StoreResult<()> {
        match self {
            Self::Local { current, legacy } => {
                current.verify_current()?;
                legacy.verify_current()
            }
            Self::Families(v) => v.verify_current(),
            Self::Desktop(v) => v.verify_current(),
            Self::Management(v) => v.verify_current(),
        }
    }
}
impl TransportRootsObservationV2 {
    pub(crate) fn management_staging_paths(&self) -> StoreResult<Vec<PathBuf>> {
        self.verify_current()?;
        let mut paths = Vec::new();
        for root in &self.roots {
            if let Some(Contents::Management(owner)) = &root.contents
                && owner.needs_staging()?
            {
                paths.push(root.path.path.clone());
            }
        }
        self.verify_current()?;
        Ok(paths)
    }
    pub(crate) fn management_staging_parent(&self, path: &Path) -> StoreResult<&File> {
        self.verify_current()?;
        let root = self
            .roots
            .iter()
            .find(|v| v.path.path == path)
            .ok_or(StoreError::RecoveryRequired)?;
        match &root.contents {
            Some(Contents::Management(owner)) => owner.staging_parent(),
            _ => Err(StoreError::RecoveryRequired),
        }
    }
    pub(crate) fn management_prepare_stage(
        &self,
        path: &Path,
    ) -> StoreResult<(&File, crate::management_effect::ManagementMigrationTreeV2)> {
        self.verify_current()?;
        let root = self
            .roots
            .iter()
            .find(|v| v.path.path == path)
            .ok_or(StoreError::RecoveryRequired)?;
        match &root.contents {
            Some(Contents::Management(owner)) => owner.prepare_stage(),
            _ => Err(StoreError::RecoveryRequired),
        }
    }
    pub(crate) fn bind_management_stage(self, path: &Path, file: File) -> StoreResult<Self> {
        self.rebind_management(path, ManagementUpdateV2::Stage(file))
    }
    pub(crate) fn finish_management_stage(
        self,
        path: &Path,
        tree: crate::management_effect::ManagementMigrationTreeV2,
    ) -> StoreResult<Self> {
        self.rebind_management(path, ManagementUpdateV2::Finished(Box::new(tree)))
    }
    pub(crate) fn management_paths(&self) -> StoreResult<Vec<PathBuf>> {
        self.verify_current()?;
        let mut paths = Vec::new();
        for root in &self.roots {
            if let Some(Contents::Management(owner)) = &root.contents
                && owner.source_target()?.is_some()
            {
                paths.push(root.path.path.clone());
            }
        }
        self.verify_current()?;
        Ok(paths)
    }
    pub(crate) fn management_migrations(
        &self,
        migration_id: &str,
    ) -> StoreResult<Vec<polaris_mcp::migration_observation::ManagementMigrationBindingV2>> {
        self.verify_current()?;
        let mut bindings = Vec::new();
        for root in &self.roots {
            if let Some(Contents::Management(owner)) = &root.contents
                && let Some(binding) = owner.migration_binding()?
            {
                if binding.migration_id != migration_id {
                    return Err(StoreError::RecoveryRequired);
                }
                bindings.push(binding);
            }
        }
        bindings.sort_by(|a, b| a.state_path.cmp(&b.state_path));
        self.verify_current()?;
        Ok(bindings)
    }
    pub(crate) fn management_target(&self, path: &Path) -> StoreResult<(&File, EntryAnchorV2)> {
        self.verify_current()?;
        let root = self
            .roots
            .iter()
            .find(|root| root.path.path == path)
            .ok_or(StoreError::RecoveryRequired)?;
        match &root.contents {
            Some(Contents::Management(owner)) => {
                owner.source_target()?.ok_or(StoreError::RecoveryRequired)
            }
            _ => Err(StoreError::RecoveryRequired),
        }
    }
    pub(crate) fn rebind_management_renamed(
        self,
        path: &Path,
        id: crate::management_effect::ManagementEffectHashV1,
    ) -> StoreResult<Self> {
        self.rebind_management(path, ManagementUpdateV2::Renamed(id))
    }
    pub(crate) fn bind_management_tombstone(self, path: &Path) -> StoreResult<Self> {
        self.rebind_management(path, ManagementUpdateV2::Tombstone)
    }
    fn rebind_management(mut self, path: &Path, update: ManagementUpdateV2) -> StoreResult<Self> {
        for root in &self.roots {
            root.path.verify_current()?;
            if root.path.path != path
                && let Some(contents) = &root.contents
            {
                contents.verify_current()?;
            }
        }
        let root = self
            .roots
            .iter_mut()
            .find(|root| root.path.path == path)
            .ok_or(StoreError::RecoveryRequired)?;
        let Some(Contents::Management(owner)) = root.contents.take() else {
            return Err(StoreError::RecoveryRequired);
        };
        root.contents = Some(Contents::Management(Box::new(match update {
            ManagementUpdateV2::Renamed(id) => owner.rebind_renamed(id)?,
            ManagementUpdateV2::Tombstone => owner.bind_tombstone()?,
            ManagementUpdateV2::Stage(file) => owner.bind_stage(file)?,
            ManagementUpdateV2::Finished(tree) => owner.finish_stage(*tree)?,
        })));
        self.verify_current()?;
        Ok(self)
    }
    pub(crate) fn management_namespace(
        &self,
        path: &Path,
    ) -> StoreResult<(std::fs::Metadata, Vec<String>)> {
        self.verify_current()?;
        let root = self
            .roots
            .iter()
            .find(|root| root.path.path == path)
            .ok_or(StoreError::RecoveryRequired)?;
        match &root.contents {
            Some(Contents::Management(owner)) => owner.namespace_snapshot(),
            _ => Err(StoreError::RecoveryRequired),
        }
    }
    pub(crate) fn inspect(supplied: &[TransportRootV2]) -> StoreResult<Self> {
        if supplied.is_empty() || supplied.len() > MAX_ROOTS {
            return Err(StoreError::RecoveryRequired);
        }
        let mut ordered = supplied.iter().collect::<Vec<_>>();
        ordered.sort_by(|a, b| a.path().cmp(b.path()));
        if ordered
            .windows(2)
            .any(|pair| pair[0].path() == pair[1].path())
        {
            return Err(StoreError::RecoveryRequired);
        }
        let mut identities = BTreeSet::new();
        let mut roots = Vec::new();
        for root in ordered {
            let path = PathPin::inspect(root.path())?;
            let contents = if path.existing() {
                if !identities.insert((path.anchor.device, path.anchor.inode)) {
                    return Err(StoreError::RecoveryRequired);
                }
                Some(match root {
                    TransportRootV2::LocalData(p) => Contents::Local {
                        current: LocalSessionsTransportInspectionV2::inspect(
                            &disk::OwnerControlledContainer::open_migration(p)?,
                        )?,
                        legacy: legacy::LegacySessionsObservation::inspect(p)?,
                    },
                    TransportRootV2::ManagerFamilies(p) => Contents::Families(
                        FamilyTransportInspectionV2::inspect(&disk::Directory::open_owned(p)?)?,
                    ),
                    TransportRootV2::ManagementProject(p) => Contents::Management(Box::new(
                        management::ManagementProjectObservationV2::inspect(p)?,
                    )),
                    TransportRootV2::Desktop(p) => {
                        Contents::Desktop(DesktopRootTransportInspectionV2::inspect(
                            &disk::Directory::open_owned(p)?,
                        )?)
                    }
                })
            } else {
                None
            };
            roots.push(RootObservation { path, contents });
        }
        #[cfg(test)]
        run_hook(&AFTER_ROOTS_INSPECTED);
        let view = Self { roots };
        view.verify_current()?;
        Ok(view)
    }
    pub(crate) fn verify_current(&self) -> StoreResult<()> {
        for root in &self.roots {
            root.path.verify_current()?;
        }
        for root in &self.roots {
            if let Some(contents) = &root.contents {
                contents.verify_current()?;
            }
        }
        // A later reader must not leave an earlier root's namespace unclosed.
        #[cfg(test)]
        run_hook(&AFTER_ROOTS_VERIFIED);
        for root in &self.roots {
            root.path.verify_current()?;
        }
        for root in &self.roots {
            if let Some(contents) = &root.contents {
                contents.verify_current()?;
            }
        }
        Ok(())
    }
    pub(crate) fn runtime_exclusions(&self) -> StoreResult<Vec<(String, Vec<PathBuf>)>> {
        self.verify_current()?;
        let paths = self
            .roots
            .iter()
            .filter_map(|root| match &root.contents {
                Some(Contents::Management(owner)) => Some(owner.runtime_exclusions()),
                _ => None,
            })
            .collect::<StoreResult<Vec<_>>>()?;
        self.verify_current()?;
        Ok(paths)
    }
}

// Retain the existing root, or the nearest existing ancestor and its first
// absent child. Rechecking absence creates nothing and never follows aliases.
struct PathPin {
    path: PathBuf,
    ancestor_path: PathBuf,
    ancestor: File,
    anchor: EntryAnchorV2,
    mount: polaris_mcp::NativeMountIdentityV2,
    absent_child: Option<String>,
}
impl PathPin {
    fn inspect(path: &Path) -> StoreResult<Self> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(StoreError::RecoveryRequired);
        }
        // Reject lexical aliases even when a missing suffix prevents the
        // existing Directory canonical-path check from reaching that suffix.
        if path
            .as_os_str()
            .as_encoded_bytes()
            .split(|b| *b == b'/')
            .skip(1)
            .any(|c| c.is_empty() || c == b"." || c == b"..")
        {
            return Err(StoreError::RecoveryRequired);
        }
        let mut candidate = path.to_owned();
        let mut absent_child = None;
        let ancestor = loop {
            match disk::OwnerControlledContainer::open_migration(&candidate) {
                Ok(directory) => break directory.retained_file_m2a()?,
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    absent_child = Some(
                        candidate
                            .file_name()
                            .and_then(|s| s.to_str())
                            .ok_or(StoreError::RecoveryRequired)?
                            .to_owned(),
                    );
                    candidate = candidate
                        .parent()
                        .ok_or(StoreError::RecoveryRequired)?
                        .to_owned();
                }
                Err(error) => return Err(error),
            }
        };
        owned_directory(&ancestor, false).map_err(|_| StoreError::RecoveryRequired)?;
        let value = Self {
            path: path.to_owned(),
            ancestor_path: candidate,
            anchor: EntryAnchorV2::of(&ancestor.metadata()?),
            mount: native_mount(&ancestor)?,
            ancestor,
            absent_child,
        };
        value.verify_current()?;
        Ok(value)
    }
    fn existing(&self) -> bool {
        self.absent_child.is_none()
    }
    fn verify_current(&self) -> StoreResult<()> {
        let named = disk::OwnerControlledContainer::open_migration(&self.ancestor_path)?
            .retained_file_m2a()?;
        for file in [&self.ancestor, &named] {
            owned_directory(file, false).map_err(|_| StoreError::RecoveryRequired)?;
            let current = EntryAnchorV2::of(&file.metadata()?);
            if (current.device, current.inode, current.uid, current.mode)
                != (
                    self.anchor.device,
                    self.anchor.inode,
                    self.anchor.uid,
                    self.anchor.mode,
                )
                || native_mount(file)? != self.mount
            {
                return Err(StoreError::RecoveryRequired);
            }
            if let Some(name) = &self.absent_child {
                match child(
                    file,
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
                ) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    _ => return Err(StoreError::RecoveryRequired),
                }
            }
        }
        if self.existing() && self.path != self.ancestor_path {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_ROOTS_INSPECTED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_ROOTS_VERIFIED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
type HookSlot = std::thread::LocalKey<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>;
#[cfg(test)]
fn run_hook(slot: &'static HookSlot) {
    if let Some(hook) = slot.with(|s| s.borrow_mut().take()) {
        hook();
    }
}
#[cfg(test)]
mod tests;
