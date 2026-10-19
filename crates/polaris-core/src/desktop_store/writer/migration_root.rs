//! Complete existing desktop-root observation; no create, repair, or activation.
use super::super::super::session_manager::migration::SessionManagerTransportInspectionV2;
use super::*;
use std::{collections::BTreeMap, fs::Metadata};

const MAX_ROOT_ENTRIES: usize = 65_536;
const MAX_ROOT_BYTES: u64 = 256 * 1024 * 1024;
const AUXILIARIES: [&str; 2] = ["subagent-control-v1", "workflow-artifacts-v1"];
pub(in crate::desktop_store) struct DesktopRootTransportInspectionV2 {
    directory: Arc<disk::Directory>,
    metadata: Metadata,
    mount: Arc<polaris_mcp::NativeMountIdentityV2>,
    names: Vec<String>,
    sessions: BTreeMap<String, DesktopSessionTransportInspectionV2>,
    manager: Option<SessionManagerTransportInspectionV2>,
    auxiliary: Vec<AuxiliaryInspection>,
}
impl DesktopRootTransportInspectionV2 {
    pub(in crate::desktop_store) fn inspect(root: &disk::Directory) -> StoreResult<Self> {
        let supplied = root.retained_file_m2a()?;
        let metadata = supplied.metadata()?;
        let mount = Arc::new(native_mount(&supplied)?);
        let directory = Arc::new(
            disk::Directory::open_owned(root.path())?.with_migration_mount(mount.clone())?,
        );
        disk::same(&supplied, &directory.retained_file_m2a()?)?;
        let names = directory.names_bounded_migration(MAX_ROOT_ENTRIES)?;
        if names.iter().any(|name| {
            !hash_name(name)
                && name != "session-manager-v1"
                && !AUXILIARIES.contains(&name.as_str())
        }) {
            return Err(StoreError::RecoveryRequired);
        }
        #[cfg(test)]
        run_hook(&AFTER_ROOT_NAMES);
        let mut remaining = MAX_ROOT_BYTES;
        let manager = if names.iter().any(|name| name == "session-manager-v1") {
            Some(SessionManagerTransportInspectionV2::inspect_bounded(
                &directory,
                &mut remaining,
            )?)
        } else {
            None
        };
        let mut sessions = BTreeMap::new();
        for name in names.iter().filter(|name| hash_name(name)) {
            let session = DesktopSessionTransportInspectionV2::inspect_bounded(
                &directory,
                name,
                &mut remaining,
            )?;
            sessions.insert(name.clone(), session);
        }
        let mut auxiliary = Vec::new();
        for name in AUXILIARIES {
            if names.iter().any(|candidate| candidate == name) {
                auxiliary.push(AuxiliaryInspection::inspect(
                    &directory,
                    name,
                    &mut remaining,
                )?);
            }
        }
        let backing = StoreRoot {
            directory: directory.clone(),
            _temporary: None,
        };
        for session in sessions.values() {
            validate_artifact_bodies_with_mode(&backing, session.verified_published()?, true)?;
        }
        #[cfg(test)]
        run_hook(&AFTER_ROOT_DECODE);
        let view = Self {
            directory,
            metadata,
            mount,
            names,
            sessions,
            manager,
            auxiliary,
        };
        view.verify_current()?;
        Ok(view)
    }
    pub(in crate::desktop_store) fn verified_sessions(
        &self,
    ) -> StoreResult<&BTreeMap<String, DesktopSessionTransportInspectionV2>> {
        self.verify_current()?;
        Ok(&self.sessions)
    }
    pub(in crate::desktop_store) fn verify_current(&self) -> StoreResult<()> {
        self.verify_root()?;
        if let Some(manager) = &self.manager {
            manager.verify_current()?;
        }
        for session in self.sessions.values() {
            session.verify_current()?;
        }
        for auxiliary in &self.auxiliary {
            auxiliary.verify_current()?;
        }
        self.verify_root()?;
        #[cfg(test)]
        run_hook(&AFTER_ROOT_FINAL_NAMES);
        self.verify_stamp()
    }
    fn verify_root(&self) -> StoreResult<()> {
        self.verify_stamp()?;
        if self.directory.names_bounded_migration(self.names.len())? != self.names {
            return Err(StoreError::RecoveryRequired);
        }
        self.verify_stamp()
    }
    fn verify_stamp(&self) -> StoreResult<()> {
        self.directory.verify()?;
        let file = self.directory.retained_file_m2a()?;
        if native_mount(&file)? != *self.mount
            || file_stamp(&file.metadata()?) != file_stamp(&self.metadata)
        {
            return Err(StoreError::RecoveryRequired);
        }
        Ok(())
    }
}

fn hash_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
struct AuxiliaryInspection {
    directories: Vec<(disk::Directory, Metadata, Vec<String>)>,
    files: Vec<(usize, String, File, Metadata)>,
    entries: usize,
}
impl AuxiliaryInspection {
    fn inspect(root: &disk::Directory, name: &str, remaining: &mut u64) -> StoreResult<Self> {
        let mut view = Self {
            directories: Vec::new(),
            files: Vec::new(),
            entries: 0,
        };
        view.visit(
            root.child(name, false)?,
            name == "subagent-control-v1",
            Vec::new(),
            remaining,
        )?;
        view.verify_current()?;
        Ok(view)
    }
    fn visit(
        &mut self,
        directory: disk::Directory,
        control: bool,
        coordinates: Vec<String>,
        remaining: &mut u64,
    ) -> StoreResult<()> {
        let metadata = directory.retained_file_m2a()?.metadata()?;
        let names = directory.names_bounded_migration(MAX_ROOT_ENTRIES)?;
        self.entries = self.entries.checked_add(1).ok_or(StoreError::Overflow)?;
        if self.entries > MAX_ROOT_ENTRIES {
            return Err(StoreError::Capacity("desktop auxiliary entries"));
        }
        let index = self.directories.len();
        self.directories.push((directory, metadata, names.clone()));
        let depth = coordinates.len();
        let leaf = if control { depth == 5 } else { depth == 2 };
        if !leaf {
            if !control && depth == 1 && !names.is_empty() && names != ["objects"] {
                return Err(StoreError::RecoveryRequired);
            }
            for name in names {
                if (control || depth == 0) && !hash_name(&name) {
                    return Err(StoreError::RecoveryRequired);
                }
                let child = self.directories[index].0.child(&name, false)?;
                let mut next = coordinates.clone();
                next.push(name);
                self.visit(child, control, next, remaining)?;
            }
            return Ok(());
        }
        if control
            && !(names.is_empty()
                || names == ["owner.lock"]
                || names == ["owner.lock", "state.json"])
        {
            return Err(StoreError::RecoveryRequired);
        }
        for name in &names {
            if !control && !name.strip_suffix(".json").is_some_and(hash_name) {
                return Err(StoreError::RecoveryRequired);
            }
            let file = self.directories[index].0.open(name, false, false)?;
            let metadata = file.metadata()?;
            if control && name == "owner.lock" {
                if metadata.len() != 0 {
                    return Err(StoreError::RecoveryRequired);
                }
                match file.try_lock() {
                    Ok(()) => (),
                    Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
                    Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
                }
            }

            self.entries = self.entries.checked_add(1).ok_or(StoreError::Overflow)?;
            if self.entries > MAX_ROOT_ENTRIES {
                return Err(StoreError::Capacity("desktop auxiliary entries"));
            }
            *remaining = remaining
                .checked_sub(metadata.len())
                .ok_or(StoreError::Capacity("desktop root inspected bytes"))?;
            self.files.push((index, name.clone(), file, metadata));
        }
        for name in names {
            if control && name == "owner.lock" {
                continue;
            }
            let maximum = if control {
                256 * 1024
            } else {
                crate::workflow::MAX_WORKFLOW_ARTIFACT_BODY_BYTES as u64
            };

            let (_, _, file, metadata) = self
                .files
                .iter()
                .find(|(directory, candidate, _, _)| *directory == index && candidate == &name)
                .ok_or(StoreError::RecoveryRequired)?;
            let bytes = read_held_bytes(file, metadata, maximum)?;
            if control {
                super::super::super::subagent_control::validate_migration_state(
                    &coordinates,
                    &bytes,
                )?;
            } else {
                super::super::super::artifacts::validate_migration_object(&name, &bytes)?;
            }
        }
        Ok(())
    }
    fn verify_current(&self) -> StoreResult<()> {
        self.verify_directories()?;
        for (index, name, file, metadata) in &self.files {
            let named = self.directories[*index].0.open(name, false, false)?;
            for current in [file, &named] {
                if file_stamp(&current.metadata()?) != file_stamp(metadata) {
                    return Err(StoreError::RecoveryRequired);
                }
            }
        }
        self.verify_directories()
    }
    fn verify_directories(&self) -> StoreResult<()> {
        for (directory, metadata, names) in &self.directories {
            directory.verify()?;
            if file_stamp(&directory.retained_file_m2a()?.metadata()?) != file_stamp(metadata)
                || directory.names_bounded_migration(names.len())? != *names
                || file_stamp(&directory.retained_file_m2a()?.metadata()?) != file_stamp(metadata)
            {
                return Err(StoreError::RecoveryRequired);
            }
        }
        Ok(())
    }
}
#[cfg(test)]
thread_local! {
    pub(in crate::desktop_store) static AFTER_ROOT_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(in crate::desktop_store) static AFTER_ROOT_DECODE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(in crate::desktop_store) static AFTER_ROOT_FINAL_NAMES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn run_hook(slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>) {
    if let Some(hook) = slot.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}
