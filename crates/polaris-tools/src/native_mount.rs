//! Native mount identity of an actual open descriptor, never an authorization.
//! Private filesystem names are neither formatted nor serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountIdentityError;
use std::{fs::File, os::fd::AsRawFd};
// Mount names remain private bytes: no Debug/Serde or caller-supplied identity.
#[cfg(target_os = "macos")]
#[derive(PartialEq, Eq)]
pub struct NativeMountIdentityV2 {
    pub(crate) fsid: [i32; 2],
    pub(crate) mounted_on: Vec<u8>,
    pub(crate) mounted_from: Vec<u8>,
    pub(crate) flags: u32,
    pub(crate) extended_flags: u32,
    pub(crate) filesystem_type: u32,
    pub(crate) filesystem_subtype: u32,
}

#[cfg(target_os = "linux")]
#[derive(PartialEq, Eq)]
pub struct NativeMountIdentityV2 {
    pub(crate) mount_id: u64,
}

impl NativeMountIdentityV2 {
    pub fn inspect(file: &File) -> Result<Self, MountIdentityError> {
        platform_mount_identity(file)
    }
}
#[cfg(target_os = "macos")]
pub(crate) fn platform_mount_identity(
    file: &File,
) -> Result<NativeMountIdentityV2, MountIdentityError> {
    // The current local sys/mount.h and libc Apple ABI define f_fsid as two
    // int32_t values. fstatfs binds these fields to the *open descriptor*.
    // Dynamic free-space/file counters are deliberately not identity fields.
    let mut information = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(file.as_raw_fd(), information.as_mut_ptr()) } != 0 {
        return Err(MountIdentityError);
    }
    let information = unsafe { information.assume_init() };
    let fsid: [i32; 2] = unsafe { std::mem::transmute(information.f_fsid) };
    if fsid == [-1, -1] {
        return Err(MountIdentityError);
    }
    Ok(NativeMountIdentityV2 {
        fsid,
        mounted_on: mount_name(&information.f_mntonname)?,
        mounted_from: mount_name(&information.f_mntfromname)?,
        flags: information.f_flags,
        extended_flags: information.f_flags_ext,
        filesystem_type: information.f_type,
        filesystem_subtype: information.f_fssubtype,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn mount_name(value: &[libc::c_char]) -> Result<Vec<u8>, MountIdentityError> {
    let end = value
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(MountIdentityError)?;
    if end == 0 {
        return Err(MountIdentityError);
    }
    Ok(value[..end].iter().map(|byte| *byte as u8).collect())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn platform_mount_identity(
    file: &File,
) -> Result<NativeMountIdentityV2, MountIdentityError> {
    // fstatfs/dev alone misses a bind mount of the same filesystem. AT_EMPTY_PATH
    // inspects the held FD; the returned mask must actually include MNT_ID.
    let mut information = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            information.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(MountIdentityError);
    }
    let information = unsafe { information.assume_init() };
    linux_mount_identity(&information)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn linux_mount_identity(
    information: &libc::statx,
) -> Result<NativeMountIdentityV2, MountIdentityError> {
    if information.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(MountIdentityError);
    }
    Ok(NativeMountIdentityV2 {
        mount_id: information.stx_mnt_id,
    })
}

#[cfg(all(target_os = "linux", not(target_env = "gnu")))]
pub(crate) fn platform_mount_identity(
    _: &File,
) -> Result<NativeMountIdentityV2, MountIdentityError> {
    // This owner has no verified libc statx ABI on other Linux environments.
    // Never fall back to a device or filesystem ID that aliases bind mounts.
    Err(MountIdentityError)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn mac_mount_name_requires_a_nonempty_terminated_identity() {
        let empty = [0 as libc::c_char; 3];
        assert!(mount_name(&empty).is_err());
        let unterminated = [b'a' as libc::c_char; 3];
        assert!(mount_name(&unterminated).is_err());
        let valid = [b'/' as libc::c_char, b'x' as libc::c_char, 0];
        assert_eq!(mount_name(&valid).unwrap(), b"/x");
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn linux_mount_identity_requires_the_kernel_returned_statx_mask() {
        let mut information = unsafe { std::mem::zeroed::<libc::statx>() };
        information.stx_mnt_id = 47;
        assert!(linux_mount_identity(&information).is_err());
        information.stx_mask = libc::STATX_MNT_ID;
        assert_eq!(linux_mount_identity(&information).unwrap().mount_id, 47);
    }

    #[test]
    fn native_mount_descriptor_identity_includes_platform_mount_binding() {
        let root = tempfile::tempdir().unwrap();
        let file = File::open(root.path()).unwrap();
        let expected = NativeMountIdentityV2::inspect(&file).unwrap();
        let mut different = NativeMountIdentityV2::inspect(&file).unwrap();
        #[cfg(target_os = "macos")]
        different.mounted_on.extend_from_slice(b"/injected-subtree");
        #[cfg(target_os = "linux")]
        {
            different.mount_id ^= 1;
        }
        assert!(different != expected);
        assert!(NativeMountIdentityV2::inspect(&file).unwrap() == expected);
    }
}
