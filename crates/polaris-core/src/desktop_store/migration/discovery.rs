//! Existing-only transport topology in every supplied data/settings location.
//! Historical custom locations still require explicit operator inventory.
use super::super::writer::migration::file_stamp;
use super::*;
use std::{collections::BTreeMap, fs::Metadata};

pub(crate) struct TransportRootDiscoveryV2 {
    locations: Vec<PathPin>,
    namespaces: Vec<Namespace>,
    roots: Vec<TransportRootV2>,
}
impl TransportRootDiscoveryV2 {
    /// Adopt only the exact project phase already closed by its retained owner.
    pub(crate) fn bind_management_phase(
        mut self,
        roots: &TransportRootsObservationV2,
        path: &Path,
    ) -> StoreResult<Self> {
        let (metadata, names) = roots.management_namespace(path)?;
        for namespace in &self.namespaces {
            if namespace.path.path != path {
                namespace.verify_current()?;
            }
        }
        let namespace = self
            .namespaces
            .iter_mut()
            .find(|v| v.path.path == path)
            .ok_or(StoreError::RecoveryRequired)?;
        namespace.path.verify_current()?;
        namespace.metadata = Some(metadata);
        namespace.names = names;
        self.verify_current()?;
        roots.verify_current()?;
        Ok(self)
    }
    pub(crate) fn inspect(data_paths: &[PathBuf], settings_files: &[PathBuf]) -> StoreResult<Self> {
        Self::inspect_bounded(data_paths, settings_files, MAX_ROOTS)
    }
    fn inspect_bounded(
        data_paths: &[PathBuf],
        settings_files: &[PathBuf],
        mut remaining_names: usize,
    ) -> StoreResult<Self> {
        if data_paths.is_empty()
            || data_paths.len().saturating_add(settings_files.len()) > MAX_ROOTS
            || remaining_names > MAX_ROOTS
        {
            return Err(StoreError::RecoveryRequired);
        }
        let mut locations = Vec::new();
        let mut namespaces = Vec::new();
        let mut roots = BTreeMap::new();
        // Exact repeat of the default path is harmless. Physical or lexical
        // aliases remain rejected by PathPin and the final root observer.
        for path in data_paths.iter().collect::<BTreeSet<_>>() {
            locations.push(PathPin::inspect(path)?);
            insert(&mut roots, TransportRootV2::LocalData(path.clone()))?;
            let state = Namespace::inspect(&path.join("state"), &mut remaining_names)?;
            for id in &state.names {
                if id.len() != 16
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(StoreError::RecoveryRequired);
                }
                let project =
                    Namespace::inspect(&path.join("state").join(id), &mut remaining_names)?;
                if !project.path.existing() {
                    return Err(StoreError::RecoveryRequired);
                }
                insert(
                    &mut roots,
                    TransportRootV2::ManagementProject(project.path.path.clone()),
                )?;
                insert(
                    &mut roots,
                    TransportRootV2::ManagerFamilies(project.path.path.join("tui-admissions-v1")),
                )?;
                namespaces.push(project);
            }
            namespaces.push(state);
        }
        for settings in settings_files.iter().collect::<BTreeSet<_>>() {
            let parent = settings.parent().ok_or(StoreError::RecoveryRequired)?;
            let name = settings
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or(StoreError::RecoveryRequired)?;
            if name.is_empty() || settings.as_os_str().as_encoded_bytes().last() == Some(&b'/') {
                return Err(StoreError::RecoveryRequired);
            }
            // The settings parent itself is not a DesktopRoot. Do not inspect
            // or create settings content merely to find its physical sibling.
            let path = parent.join(format!("{name}.workspace"));
            locations.push(PathPin::inspect(&path)?);
            insert(&mut roots, TransportRootV2::Desktop(path))?;
        }
        #[cfg(test)]
        run_hook(&AFTER_DISCOVERY);
        let view = Self {
            locations,
            namespaces,
            roots: roots.into_values().collect(),
        };
        view.verify_current()?;
        Ok(view)
    }
    pub(crate) fn verified_roots(&self) -> StoreResult<&[TransportRootV2]> {
        self.verify_current()?;
        Ok(&self.roots)
    }
    pub(crate) fn verify_current(&self) -> StoreResult<()> {
        for path in &self.locations {
            path.verify_current()?;
        }
        for namespace in &self.namespaces {
            namespace.verify_current()?;
        }
        // Project views are inspected before the outer state census; close
        // them again after that census so later readers cannot add a family.
        #[cfg(test)]
        run_hook(&AFTER_DISCOVERY_VERIFY);
        for namespace in &self.namespaces {
            namespace.verify_current()?;
        }
        for path in &self.locations {
            path.verify_current()?;
        }
        Ok(())
    }
}
fn insert(
    roots: &mut BTreeMap<PathBuf, TransportRootV2>,
    root: TransportRootV2,
) -> StoreResult<()> {
    if roots.len() >= MAX_ROOTS || roots.contains_key(root.path()) {
        return Err(StoreError::RecoveryRequired);
    }
    roots.insert(root.path().to_owned(), root);
    Ok(())
}

struct Namespace {
    path: PathPin,
    metadata: Option<Metadata>,
    names: Vec<String>,
}
impl Namespace {
    fn inspect(path: &Path, remaining: &mut usize) -> StoreResult<Self> {
        let path = PathPin::inspect(path)?;
        let (metadata, names) = if path.existing() {
            let directory = disk::OwnerControlledContainer::open_migration(&path.path)?;
            let file = directory.retained_file_m2a()?;
            let metadata = file.metadata()?;
            // Bound allocation across every supplied root, not once per
            // project. The reader stops before retaining an oversized census.
            let names = directory.names_bounded_migration(*remaining)?;
            *remaining = remaining
                .checked_sub(names.len())
                .ok_or(StoreError::Capacity("transport discovery names"))?;
            if file_stamp(&metadata) != file_stamp(&file.metadata()?) {
                return Err(StoreError::RecoveryRequired);
            }
            (Some(metadata), names)
        } else {
            (None, Vec::new())
        };
        let view = Self {
            path,
            metadata,
            names,
        };
        view.verify_current()?;
        Ok(view)
    }
    fn verify_current(&self) -> StoreResult<()> {
        self.path.verify_current()?;
        if let Some(expected) = &self.metadata {
            let directory = disk::OwnerControlledContainer::open_migration(&self.path.path)?;
            let file = directory.retained_file_m2a()?;
            if file_stamp(expected) != file_stamp(&file.metadata()?)
                || directory.names_bounded_migration(self.names.len())? != self.names
            {
                return Err(StoreError::RecoveryRequired);
            }
            #[cfg(test)]
            run_hook(&AFTER_NAMESPACE_NAMES);
            if file_stamp(expected) != file_stamp(&file.metadata()?) {
                return Err(StoreError::RecoveryRequired);
            }
        }
        self.path.verify_current()
    }
}
#[cfg(test)]
thread_local! {
    static AFTER_DISCOVERY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_DISCOVERY_VERIFY: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_NAMESPACE_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
