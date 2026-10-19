//! Fixed logical-final to inventoried-staging descriptor mapping. This is a
//! read capability only: it neither proves process stopping nor authorizes
//! epoch publication, rename, or entry activation.
use super::{
    inventory::CompleteTreeProjectionV2,
    namespace::{MigrationParentObservationV2, same_parent},
    records::StagingRecordV2,
};
use crate::{
    Error,
    entry_gate::{EntryAnchorV2, child},
};
use polaris_tools::path_policy::tree_admission::{Limits, RegisteredDirectory};
use std::{
    fs::File,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
};

/// Retains a physical namespace observation and a complete stage inventory.
/// The host owner separately rechecks its exclusive gate before/after admission.
/// There is no resolver callback, serialized witness or live-final
/// fallback; a missing relative entry is always a protection failure.
pub struct MappedStagingViewV2<'a> {
    namespace: MigrationParentObservationV2,
    record: &'a StagingRecordV2,
    inventory: &'a CompleteTreeProjectionV2,
    staging: File,
    logical_root: PathBuf,
}

impl<'a> MappedStagingViewV2<'a> {
    /// Observes actual descriptors; it neither accepts a stop witness nor grants
    /// activation. The record contributes only validated layout/anchor data.
    pub fn inspect_descriptor(
        namespace: MigrationParentObservationV2,
        record: &'a StagingRecordV2,
        inventory: &'a CompleteTreeProjectionV2,
    ) -> Result<Self, Error> {
        record.validate()?;
        inventory.require_activation_tree()?;
        namespace.verify_current()?;
        if !same_parent(
            &record.canonical_parent_anchor,
            &EntryAnchorV2::of(&namespace.parent().metadata().map_err(|_| Error::Storage)?),
        ) {
            return Err(Error::Protection);
        }
        let layout = &record.retired.binding.layout;
        require_absent(namespace.parent(), &layout.active.component())?;
        let staging = child(
            namespace.parent(),
            &layout.staging.component(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| Error::Protection)?;
        if EntryAnchorV2::of(&staging.metadata().map_err(|_| Error::Storage)?)
            != record.staging_anchor
        {
            return Err(Error::Protection);
        }
        inventory.verify_same_mount(namespace.parent())?;
        inventory.verify_root_mount_binding(&staging)?;
        let logical_root = namespace
            .logical_home()
            .join(".polaris")
            .join(layout.active.component());
        let value = Self {
            namespace,
            record,
            inventory,
            staging,
            logical_root,
        };
        value.revalidate()?;
        Ok(value)
    }

    pub fn logical_root(&self) -> &Path {
        &self.logical_root
    }

    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.logical_root)
    }

    /// Run the same strict directory walker as ordinary startup, anchored to
    /// descriptors from this fixed map. The physical path below is only a
    /// protection-policy/diagnostic coordinate; no lookup resolves that string.
    pub fn registered_directory(
        &self,
        path: &Path,
        limits: Limits,
    ) -> Result<RegisteredDirectory, Error> {
        let root = self.open_readonly(path)?;
        let relative = path
            .strip_prefix(&self.logical_root)
            .map_err(|_| Error::Protection)?;
        let stage_name = self.record.retired.binding.layout.staging.component();
        self.namespace.verify_current()?;
        let home = self.namespace.logical_home();
        let physical = home.join(".polaris").join(&stage_name).join(relative);
        let (parent, name) = if relative.as_os_str().is_empty() {
            (
                self.namespace
                    .parent()
                    .try_clone()
                    .map_err(|_| Error::Storage)?,
                std::ffi::OsString::from(stage_name),
            )
        } else {
            (
                self.open_readonly(path.parent().ok_or(Error::Protection)?)?,
                path.file_name().ok_or(Error::Protection)?.to_owned(),
            )
        };
        let witness =
            RegisteredDirectory::inspect_descriptor(root, parent, &name, &physical, path, limits)
                .map_err(|_| Error::Protection)?;
        self.revalidate()?;
        Ok(witness)
    }

    /// Only exact normalized paths below the logical final root are mapped.
    /// Foreign paths belong to the validator's explicit ordinary-live branch.
    pub fn open_readonly(&self, path: &Path) -> Result<File, Error> {
        self.verify_binding()?;
        let normalized: PathBuf = path.components().collect();
        if !path.is_absolute()
            || normalized.as_os_str() != path.as_os_str()
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(Error::Protection);
        }
        let relative = path
            .strip_prefix(&self.logical_root)
            .map_err(|_| Error::Protection)?;
        let copy = self.inventory.verify_before_copy()?;
        let file = if relative.as_os_str().is_empty() {
            self.staging.try_clone().map_err(|_| Error::Storage)?
        } else {
            let entry = copy
                .entries()
                .find(|entry| entry.relative_path() == relative.as_os_str().as_bytes())
                .ok_or(Error::Protection)?;
            entry.open_readonly()?
        };
        copy.verify_after_copy()?;
        self.verify_binding()?;
        Ok(file)
    }

    /// Read-only install-root admission using the same logical topology,
    /// protection registry, owner/mode rules and bounds as live admission.
    /// All physical entries come from the complete no-link stage inventory.
    pub fn admit_read_root(
        &self,
        root: &Path,
        home: &Path,
        cwd: &Path,
        excluded: &[PathBuf],
    ) -> Result<(), Error> {
        crate::admission::validate_read_root_location(root, home, cwd, excluded)?;
        let relative = root
            .strip_prefix(&self.logical_root)
            .map_err(|_| Error::Protection)?;
        self.namespace.verify_current()?;
        let actual_home = self.namespace.logical_home();
        let stage = actual_home
            .join(".polaris")
            .join(self.record.retired.binding.layout.staging.component());
        let physical_root = stage.join(relative);
        let (paths, identities) = polaris_auth::protection::with_protected_paths(|registered| {
            (registered.paths().clone(), registered.identities().clone())
        })
        .map_err(|_| Error::Protection)?;
        if paths
            .iter()
            .any(|p| p.starts_with(root) || p.starts_with(&physical_root))
        {
            return Err(Error::Protection);
        }
        let inspect = |logical: &Path, physical: &Path, file: &File| -> Result<(), Error> {
            let metadata = file.metadata().map_err(|_| Error::Storage)?;
            crate::admission::validate_read_root_node(logical, &metadata, &identities)?;
            crate::admission::validate_read_root_node(physical, &metadata, &identities)?;
            if metadata.mode() & 0o022 != 0 {
                return Err(Error::Protection);
            }
            Ok(())
        };
        inspect(root, &physical_root, &self.open_readonly(root)?)?;
        let copy = self.inventory.verify_before_copy()?;
        let mut seen = 1;
        for entry in copy.entries() {
            let path = Path::new(std::ffi::OsStr::from_bytes(entry.relative_path()));
            let Ok(suffix) = path.strip_prefix(relative) else {
                continue;
            };
            if suffix.as_os_str().is_empty() {
                continue;
            }
            seen += 1;
            crate::admission::validate_read_root_count(seen)?;
            if suffix.components().count() > crate::admission::MAX_ROOT_DEPTH {
                return Err(Error::Limit);
            }
            inspect(
                &root.join(suffix),
                &physical_root.join(suffix),
                &entry.open_readonly()?,
            )?;
        }
        copy.verify_after_copy()?;
        self.revalidate()
    }

    /// Apply the ordinary runtime's bounded regular-file and same-FD digest
    /// checks, then close the complete stage inventory and namespace binding. No
    /// logical-final filesystem lookup or fallback is performed.
    pub fn pinned_file(
        &self,
        path: &Path,
        expected: Option<&String>,
    ) -> Result<(PathBuf, File, std::fs::Metadata), Error> {
        let file = self.open_readonly(path)?;
        let pin = crate::admission::inspect_pinned_file(path, file, expected)?;
        self.revalidate()?;
        Ok(pin)
    }

    /// Reopen through the same immutable map before trusting a retained pin.
    /// This closes name replacement, descriptor mutation and unrelated tree
    /// mutation together; an ordinary path-based pin cannot be substituted.
    pub fn revalidate_pin(&self, pin: &(PathBuf, File, std::fs::Metadata)) -> Result<(), Error> {
        let named = self.open_readonly(&pin.0)?;
        if !crate::admission::same_metadata(&named.metadata().map_err(|_| Error::Storage)?, &pin.2)
            || !crate::admission::same_metadata(
                &pin.1.metadata().map_err(|_| Error::Storage)?,
                &pin.2,
            )
        {
            return Err(Error::Protection);
        }
        self.revalidate()
    }

    pub fn verify_current(&self) -> Result<(), Error> {
        self.revalidate()
    }

    pub fn revalidate(&self) -> Result<(), Error> {
        self.verify_binding()?;
        self.inventory.verify_before_copy()?.verify_after_copy()?;
        self.verify_binding()
    }

    fn verify_binding(&self) -> Result<(), Error> {
        self.namespace.verify_current()?;
        self.record.validate()?;
        let binding = &self.record.retired.binding;
        if !same_parent(
            &self.record.canonical_parent_anchor,
            &EntryAnchorV2::of(
                &self
                    .namespace
                    .parent()
                    .metadata()
                    .map_err(|_| Error::Storage)?,
            ),
        ) {
            return Err(Error::Protection);
        }
        require_absent(self.namespace.parent(), &binding.layout.active.component())?;
        let named = child(
            self.namespace.parent(),
            &binding.layout.staging.component(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| Error::Protection)?;
        for file in [&self.staging, &named] {
            if EntryAnchorV2::of(&file.metadata().map_err(|_| Error::Storage)?)
                != self.record.staging_anchor
            {
                return Err(Error::Protection);
            }
            self.inventory.verify_root_mount_binding(file)?;
        }
        self.namespace.verify_current()
    }
}

fn require_absent(parent: &File, name: &str) -> Result<(), Error> {
    match child(parent, name, libc::O_RDONLY | libc::O_NONBLOCK) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(Error::Protection),
    }
}
#[cfg(test)]
mod tests;
