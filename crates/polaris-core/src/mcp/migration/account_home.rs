//! Retained OS account-home identity and descriptor binding for migration.
// Core-private retained home capability; OS lookup lives in the shared kernel.
pub use polaris_tools::account_home::{TrustedHomeError, trusted_user_home};
use std::{
    fs::File,
    path::{Path, PathBuf},
};
/// Private retained account-home identity for the physical migration owner.
/// Neither caller paths nor environment values construct this capability.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) struct TrustedUserHomeV2 {
    path: PathBuf,
    uid: u32,
    file: File,
    anchor: polaris_tools::entry_kernel::EntryAnchorV2,
    mount: polaris_mcp::NativeMountIdentityV2,
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
impl TrustedUserHomeV2 {
    pub(crate) fn inspect() -> Result<Self, TrustedHomeError> {
        use polaris_tools::entry_kernel::{EntryAnchorV2, owned_directory, physical_directory};
        let path = account_home()?;
        let uid = unsafe { libc::geteuid() };
        let file = physical_directory(&path).map_err(|_| TrustedHomeError::Unavailable)?;
        owned_directory(&file, false).map_err(|_| TrustedHomeError::Unavailable)?;
        let anchor =
            EntryAnchorV2::of(&file.metadata().map_err(|_| TrustedHomeError::Unavailable)?);
        let mount = polaris_mcp::NativeMountIdentityV2::inspect(&file)
            .map_err(|_| TrustedHomeError::Unavailable)?;
        let home = Self {
            path,
            uid,
            file,
            anchor,
            mount,
        };
        home.verify_current()?;
        Ok(home)
    }
    pub(crate) fn verify_current(&self) -> Result<(), TrustedHomeError> {
        use polaris_tools::entry_kernel::{EntryAnchorV2, owned_directory, physical_directory};
        if unsafe { libc::geteuid() } != self.uid || account_home()? != self.path {
            return Err(TrustedHomeError::Unavailable);
        }
        let named = physical_directory(&self.path).map_err(|_| TrustedHomeError::Unavailable)?;
        for file in [&self.file, &named] {
            owned_directory(file, false).map_err(|_| TrustedHomeError::Unavailable)?;
            let anchor =
                EntryAnchorV2::of(&file.metadata().map_err(|_| TrustedHomeError::Unavailable)?);
            // Child creation changes a directory link count. Home identity
            // remains bound to dev/inode/uid/mode, not mutable membership.
            if (anchor.device, anchor.inode, anchor.uid, anchor.mode)
                != (
                    self.anchor.device,
                    self.anchor.inode,
                    self.anchor.uid,
                    self.anchor.mode,
                )
                || polaris_mcp::NativeMountIdentityV2::inspect(file)
                    .map_err(|_| TrustedHomeError::Unavailable)?
                    != self.mount
            {
                return Err(TrustedHomeError::Unavailable);
            }
        }
        if unsafe { libc::geteuid() } != self.uid || account_home()? != self.path {
            return Err(TrustedHomeError::Unavailable);
        }
        Ok(())
    }
    pub(crate) fn verified_path(&self) -> Result<&Path, TrustedHomeError> {
        self.verify_current()?;
        Ok(&self.path)
    }
    pub(crate) fn verified_directory(&self) -> Result<File, TrustedHomeError> {
        self.verify_current()?;
        let file = self
            .file
            .try_clone()
            .map_err(|_| TrustedHomeError::Unavailable)?;
        self.verify_current()?;
        Ok(file)
    }
    pub(crate) fn verified_polaris_parent(&self) -> Result<File, TrustedHomeError> {
        let home = self.verified_directory()?;
        #[cfg(test)]
        run_parent_hook(&AFTER_PARENT_HOME_FD);
        let parent = polaris_tools::entry_kernel::child(
            &home,
            ".polaris",
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(|_| TrustedHomeError::Unavailable)?;
        #[cfg(test)]
        run_parent_hook(&AFTER_PARENT_OPEN);
        self.verify_parent(&parent)?;
        self.verify_current()?;
        Ok(parent)
    }
    pub(crate) fn verify_parent(&self, parent: &File) -> Result<(), TrustedHomeError> {
        polaris_tools::entry_kernel::owned_directory(parent, false)
            .map_err(|_| TrustedHomeError::Unavailable)?;
        if polaris_mcp::NativeMountIdentityV2::inspect(parent)
            .map_err(|_| TrustedHomeError::Unavailable)?
            != self.mount
        {
            return Err(TrustedHomeError::Unavailable);
        }
        self.verify_current()
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn account_home() -> Result<PathBuf, TrustedHomeError> {
    #[cfg(test)]
    if let Some(path) = TEST_ACCOUNT_HOME.with(|slot| slot.borrow().clone()) {
        return Ok(path);
    }
    trusted_user_home()
}
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
thread_local! {
    static TEST_ACCOUNT_HOME: std::cell::RefCell<Option<PathBuf>> = const {std::cell::RefCell::new(None)};
    static AFTER_PARENT_HOME_FD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
    static AFTER_PARENT_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
}
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
fn run_parent_hook(
    slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>,
) {
    if let Some(hook) = slot.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(crate) fn with_fixture_home<T>(home: &Path, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_ACCOUNT_HOME.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TEST_ACCOUNT_HOME.with(|slot| slot.replace(Some(home.into()))));
    action()
}

#[cfg(test)]
mod tests;
