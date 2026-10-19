//! Compatibility adapter for the shared native descriptor mount identity.
use crate::Error;
use std::fs::File;
#[derive(PartialEq, Eq)]
pub struct NativeMountIdentityV2 {
    inner: polaris_tools::native_mount::NativeMountIdentityV2,
    #[cfg(test)]
    injected_difference: bool,
}
impl NativeMountIdentityV2 {
    pub fn inspect(file: &File) -> Result<Self, Error> {
        let inner = polaris_tools::native_mount::NativeMountIdentityV2::inspect(file)
            .map_err(|_| Error::Protection)?;
        Ok(Self {
            inner,
            #[cfg(test)]
            injected_difference: false,
        })
    }
    #[cfg(test)]
    pub(crate) fn inject_difference(&mut self) {
        self.injected_difference = true;
    }
}
pub(crate) fn platform_mount_identity(file: &File) -> Result<NativeMountIdentityV2, Error> {
    NativeMountIdentityV2::inspect(file)
}
