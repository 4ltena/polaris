//! Process-wide metadata-only protection for authentication stores.
//!
//! This coordinates cooperating loaders and workspace snapshots, not arbitrary
//! external filesystem writers. Entries are never evicted during the process.

use std::collections::BTreeSet;
use std::fs::File;
#[cfg(unix)]
use std::fs::{Metadata, OpenOptions};
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::AuthError;

#[cfg(windows)]
pub use windows::{
    WindowsFamilyDirectory, WindowsFamilyEntry, WindowsFamilyFile, WindowsFamilyMetadata,
};
#[cfg(windows)]
pub use windows::{
    WindowsFileIdentity, WindowsFileLock, WindowsPinnedDirectory, WindowsPinnedFile,
    WindowsPrivateRoot,
};
#[cfg(windows)]
pub use windows::{WindowsSqliteFile, WindowsSqliteMapping, WindowsSqliteRangeLock};

/// Native primitives only; the ordinary authentication entry points remain gated.
#[cfg(windows)]
mod windows {
    use super::AuthError;
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::{Arc, Mutex};
    use windows_sys::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, ERROR_LOCK_VIOLATION, ERROR_NO_TOKEN, HANDLE,
        INVALID_HANDLE_VALUE, LocalFree, UNICODE_STRING,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetSecurityDescriptorLength,
        GetTokenInformation, IsValidSecurityDescriptor, OWNER_SECURITY_INFORMATION, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BEGIN, FILE_END_OF_FILE_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
        FILE_ID_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileEndOfFileInfo,
        FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdInfo, FlushFileBuffers,
        GetDiskFreeSpaceW, GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileSizeEx,
        GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
        GetVolumeNameForVolumeMountPointW, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        LockFileEx, OPEN_EXISTING, ReadFile, SetFileInformationByHandle, SetFilePointerEx,
        UnlockFileEx, VOLUME_NAME_GUID, WriteFile,
    };
    use windows_sys::Win32::System::IO::{IO_STATUS_BLOCK, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
    };

    const DIRECTORY_ACCESS: u32 = 0x0010_00a0; // SYNCHRONIZE, attributes, traverse
    const PRIVATE_DIRECTORY_ACCESS: u32 = DIRECTORY_ACCESS | 0x0002_0000; // READ_CONTROL only on private state
    const READ_ACCESS: u32 = 0x0012_0081;
    const STAGED_ACCESS: u32 = 0x0013_0183; // read/write, attributes, DELETE, READ_CONTROL, SYNCHRONIZE
    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const OPEN_NO_REPARSE_SYNC: u32 = 0x0020_0020;
    const DIRECTORY: u32 = 1;
    const NON_DIRECTORY: u32 = 0x40;
    const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
    const MAX_SECURITY_BYTES: usize = 65_536;
    const SYSTEM_SID: &[u8] = &[1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];
    const ADMINISTRATORS_SID: &[u8] = &[1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0];

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut UNICODE_STRING,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct RenameInformation {
        replace_if_exists: u8,
        root_directory: HANDLE,
        file_name_length: u32,
        file_name: [u16; 1],
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            handle: *mut HANDLE,
            access: u32,
            attributes: *mut ObjectAttributes,
            status: *mut IO_STATUS_BLOCK,
            allocation: *const i64,
            file_attributes: u32,
            share: u32,
            disposition: u32,
            options: u32,
            ea: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn NtSetInformationFile(
            handle: HANDLE,
            status: *mut IO_STATUS_BLOCK,
            information: *const c_void,
            length: u32,
            class: i32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
        fn NtFlushBuffersFile(handle: HANDLE, status: *mut IO_STATUS_BLOCK) -> i32;
    }

    fn native_error() -> AuthError {
        std::io::Error::last_os_error().into()
    }

    fn nt_result(status: i32) -> Result<(), AuthError> {
        // Synchronous calls must complete, rather than returning STATUS_PENDING.
        if status == 0 {
            Ok(())
        } else {
            Err(
                std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
                    .into(),
            )
        }
    }

    fn wide(value: &OsStr) -> Result<Vec<u16>, AuthError> {
        let mut result: Vec<u16> = value.encode_wide().collect();
        if result.is_empty() || result.contains(&0) || result.len() > 16_000 {
            return Err(AuthError::Protection);
        }
        result.push(0);
        Ok(result)
    }

    fn component(value: &OsStr) -> Result<Vec<u16>, AuthError> {
        let text = value.to_str().ok_or(AuthError::Protection)?;
        if text.is_empty()
            || text == "."
            || text == ".."
            || text.ends_with(['.', ' '])
            || text.chars().any(|c| {
                c < ' ' || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            })
        {
            return Err(AuthError::Protection);
        }
        let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            return Err(AuthError::Protection);
        }
        let result = wide(value)?;
        if result.len() > 256 {
            return Err(AuthError::Protection);
        }
        Ok(result)
    }

    fn relative(
        parent: &WindowsPinnedDirectory,
        name: &OsStr,
        directory: bool,
        creating: bool,
        access: u32,
        descriptor: *mut c_void,
    ) -> Result<OwnedHandle, AuthError> {
        relative_shared(
            parent,
            name,
            directory,
            creating,
            access,
            descriptor,
            if directory {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            } else {
                FILE_SHARE_READ
            },
        )
    }

    fn relative_shared(
        parent: &WindowsPinnedDirectory,
        name: &OsStr,
        directory: bool,
        creating: bool,
        access: u32,
        descriptor: *mut c_void,
        share: u32,
    ) -> Result<OwnedHandle, AuthError> {
        let mut name = component(name)?;
        parent.revalidate()?;
        let mut unicode = UNICODE_STRING {
            Length: ((name.len() - 1) * 2) as u16,
            MaximumLength: (name.len() * 2) as u16,
            Buffer: name.as_mut_ptr(),
        };
        let mut attributes = ObjectAttributes {
            length: size_of::<ObjectAttributes>() as u32,
            root_directory: parent.as_raw_handle(),
            object_name: &mut unicode,
            attributes: 0x1040, // OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE
            security_descriptor: descriptor,
            security_quality_of_service: null_mut(),
        };
        let mut handle = null_mut();
        let mut status = unsafe { zeroed() };
        let result = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &mut attributes,
                &mut status,
                null(),
                0,
                share,
                if creating { FILE_CREATE } else { FILE_OPEN },
                OPEN_NO_REPARSE_SYNC | if directory { DIRECTORY } else { NON_DIRECTORY },
                null(),
                0,
            )
        };
        nt_result(result)?;
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(AuthError::Protection);
        }
        // Successful handles are owned before any further fallible validation.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        if status.Information != if creating { 2 } else { 1 } {
            return Err(AuthError::Protection);
        }
        identity(handle.as_raw_handle(), directory)?;
        parent.revalidate()?;
        Ok(handle)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct WindowsFileIdentity {
        volume: u64,
        file_id: [u8; 16],
    }

    impl WindowsFileIdentity {
        pub fn same_volume(self, other: Self) -> bool {
            self.volume == other.volume
        }
    }

    fn identity(handle: HANDLE, directory: bool) -> Result<WindowsFileIdentity, AuthError> {
        let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
        let mut id: FILE_ID_INFO = unsafe { zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } == 0
            || unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FileIdInfo,
                    (&mut id as *mut FILE_ID_INFO).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            } == 0
        {
            return Err(native_error());
        }
        if tag.ReparseTag != 0
            || tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
            || id.VolumeSerialNumber == 0
            || id.FileId.Identifier == [0; 16]
        {
            return Err(AuthError::Protection);
        }
        Ok(WindowsFileIdentity {
            volume: id.VolumeSerialNumber,
            file_id: id.FileId.Identifier,
        })
    }

    fn final_guid(handle: HANDLE) -> Result<Vec<u16>, AuthError> {
        let mut buffer = vec![0; 16_384];
        let len = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                VOLUME_NAME_GUID,
            )
        };
        if len == 0 {
            return Err(native_error());
        }
        if len as usize >= buffer.len() {
            return Err(AuthError::Protection);
        }
        buffer.truncate(len as usize);
        let prefix: Vec<u16> = "\\\\?\\Volume{".encode_utf16().collect();
        if !buffer.starts_with(&prefix) {
            return Err(AuthError::Protection);
        }
        let end = buffer
            .iter()
            .position(|c| *c == b'}' as u16)
            .ok_or(AuthError::Protection)?;
        if buffer.get(end + 1) != Some(&(b'\\' as u16)) {
            return Err(AuthError::Protection);
        }
        buffer.truncate(end + 2);
        Ok(buffer)
    }

    fn require_filesystem(handle: HANDLE) -> Result<(), AuthError> {
        let mut flags = 0;
        let mut name = [0; 32];
        if unsafe {
            GetVolumeInformationByHandleW(
                handle,
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                &mut flags,
                name.as_mut_ptr(),
                name.len() as u32,
            )
        } == 0
        {
            return Err(native_error());
        }
        let len = name
            .iter()
            .position(|c| *c == 0)
            .ok_or(AuthError::Protection)?;
        // Native acceptance currently qualifies NTFS only, not every ACL filesystem.
        if String::from_utf16(&name[..len]).ok().as_deref() != Some("NTFS") || flags & 8 == 0 {
            return Err(AuthError::Protection);
        }
        Ok(())
    }

    struct DirectoryPin {
        handle: OwnedHandle,
        identity: WindowsFileIdentity,
        volume_guid: Vec<u16>,
        parent: Option<WindowsPinnedDirectory>,
    }

    #[derive(Clone)]
    pub struct WindowsPinnedDirectory(Arc<DirectoryPin>);

    impl AsHandle for WindowsPinnedDirectory {
        fn as_handle(&self) -> BorrowedHandle<'_> {
            self.0.handle.as_handle()
        }
    }
    impl AsRawHandle for WindowsPinnedDirectory {
        fn as_raw_handle(&self) -> HANDLE {
            self.0.handle.as_raw_handle()
        }
    }

    impl WindowsPinnedDirectory {
        pub fn open_existing(path: &Path) -> Result<Self, AuthError> {
            Self::open_existing_access(path, false)
        }

        fn open_existing_access(path: &Path, private_leaf: bool) -> Result<Self, AuthError> {
            // Parse original input, not a canonicalized path that hides a reparse point.
            let original = path.as_os_str().to_str().ok_or(AuthError::Protection)?;
            let text = original.strip_prefix("\\\\?\\").unwrap_or(original);
            let bytes = text.as_bytes();
            if bytes.len() < 3
                || !bytes[0].is_ascii_alphabetic()
                || bytes[1] != b':'
                || bytes[2] != b'\\'
                || text.contains('/')
            {
                return Err(AuthError::Protection);
            }
            let mut mount: Vec<u16> = text[..3].encode_utf16().collect();
            mount.push(0);
            let mut guid = [0; 64];
            if unsafe {
                GetVolumeNameForVolumeMountPointW(
                    mount.as_ptr(),
                    guid.as_mut_ptr(),
                    guid.len() as u32,
                )
            } == 0
            {
                return Err(native_error());
            }
            let len = guid
                .iter()
                .position(|c| *c == 0)
                .ok_or(AuthError::Protection)?;
            let raw = unsafe {
                CreateFileW(
                    guid.as_ptr(),
                    DIRECTORY_ACCESS,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    null_mut(),
                )
            };
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                return Err(native_error());
            }
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            require_filesystem(handle.as_raw_handle())?;
            let id = identity(handle.as_raw_handle(), true)?;
            let actual_guid = final_guid(handle.as_raw_handle())?;
            if actual_guid != guid[..len] {
                return Err(AuthError::Protection);
            }
            let mut pin = Self(Arc::new(DirectoryPin {
                handle,
                identity: id,
                volume_guid: actual_guid,
                parent: None,
            }));
            let suffix = &text[3..];
            if !suffix.is_empty() {
                let suffix = suffix.strip_suffix('\\').unwrap_or(suffix);
                if suffix.is_empty() {
                    return Err(AuthError::Protection);
                }
                let mut names = suffix.split('\\').peekable();
                while let Some(name) = names.next() {
                    let access = if private_leaf && names.peek().is_none() {
                        PRIVATE_DIRECTORY_ACCESS
                    } else {
                        DIRECTORY_ACCESS
                    };
                    pin = pin.from_child(relative(
                        &pin,
                        OsStr::new(name),
                        true,
                        false,
                        access,
                        null_mut(),
                    )?)?;
                }
            } else if private_leaf {
                return Err(AuthError::Protection);
            }
            pin.revalidate()?;
            Ok(pin)
        }

        pub fn identity(&self) -> WindowsFileIdentity {
            self.0.identity
        }

        pub fn matches_directory_file(&self, file: &File) -> Result<bool, AuthError> {
            self.revalidate()?;
            let same = identity(file.as_raw_handle(), true)? == self.identity()
                && final_guid(file.as_raw_handle())? == self.0.volume_guid;
            self.revalidate()?;
            Ok(same)
        }

        pub fn revalidate(&self) -> Result<(), AuthError> {
            if let Some(parent) = &self.0.parent {
                parent.revalidate()?;
            }
            if identity(self.as_raw_handle(), true)? != self.0.identity
                || final_guid(self.as_raw_handle())? != self.0.volume_guid
            {
                return Err(AuthError::Protection);
            }
            Ok(())
        }

        fn from_child(&self, handle: OwnedHandle) -> Result<Self, AuthError> {
            let id = identity(handle.as_raw_handle(), true)?;
            let guid = final_guid(handle.as_raw_handle())?;
            if !id.same_volume(self.identity()) || guid != self.0.volume_guid {
                return Err(AuthError::Protection);
            }
            let child = Self(Arc::new(DirectoryPin {
                handle,
                identity: id,
                volume_guid: guid,
                parent: Some(self.clone()),
            }));
            child.revalidate()?;
            Ok(child)
        }

        pub fn child(&self, name: &OsStr) -> Result<Self, AuthError> {
            self.from_child(relative(
                self,
                name,
                true,
                false,
                DIRECTORY_ACCESS,
                null_mut(),
            )?)
        }

        pub fn open_file(&self, name: &OsStr) -> Result<WindowsPinnedFile, AuthError> {
            WindowsPinnedFile::new(
                self,
                relative(self, name, false, false, READ_ACCESS, null_mut())?,
            )
        }
    }

    struct LocalAllocation(*mut c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }

    fn bounded(base: usize, length: usize, address: usize, size: usize) -> Result<(), AuthError> {
        if address < base
            || address.checked_add(size).ok_or(AuthError::Protection)?
                > base.checked_add(length).ok_or(AuthError::Protection)?
        {
            return Err(AuthError::Protection);
        }
        Ok(())
    }

    unsafe fn sid_bytes(
        base: usize,
        size: usize,
        pointer: *const c_void,
    ) -> Result<Vec<u8>, AuthError> {
        let address = pointer as usize;
        bounded(base, size, address, 8)?;
        let header = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), 8) };
        if header[0] != 1 || header[1] > 15 {
            return Err(AuthError::Protection);
        }
        let len = 8 + 4 * header[1] as usize;
        bounded(base, size, address, len)?;
        Ok(unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), len) }.to_vec())
    }

    fn current_sid() -> Result<Vec<u8>, AuthError> {
        let mut raw = null_mut();
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut raw) } == 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
                return Err(native_error());
            }
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
                return Err(native_error());
            }
        }
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(AuthError::Protection);
        }
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut required = 0;
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                null_mut(),
                0,
                &mut required,
            )
        } != 0
            || std::io::Error::last_os_error().raw_os_error()
                != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            || required < size_of::<TOKEN_USER>() as u32
            || required as usize > MAX_SECURITY_BYTES
        {
            return Err(AuthError::Protection);
        }
        let mut buffer = vec![0usize; (required as usize).div_ceil(size_of::<usize>())];
        let capacity = buffer.len() * size_of::<usize>();
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                capacity as u32,
                &mut required,
            )
        } == 0
        {
            return Err(native_error());
        }
        if required < size_of::<TOKEN_USER>() as u32 || required as usize > capacity {
            return Err(AuthError::Protection);
        }
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        unsafe { sid_bytes(buffer.as_ptr() as usize, required as usize, user.User.Sid) }
    }

    fn private_descriptor(user: &[u8]) -> Result<LocalAllocation, AuthError> {
        let mut text = null_mut();
        if unsafe { ConvertSidToStringSidW(user.as_ptr() as *mut c_void, &mut text) } == 0 {
            return Err(native_error());
        }
        let allocation = LocalAllocation(text.cast());
        if allocation.0.is_null() {
            return Err(AuthError::Protection);
        }
        let mut length = 0;
        while length < 256 && unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        if length == 256 {
            return Err(AuthError::Protection);
        }
        let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })
            .map_err(|_| AuthError::Protection)?;
        // Token-derived SID and descriptor are never included in diagnostics.
        let sddl = format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;BA)");
        let sddl = wide(OsStr::new(&sddl))?;
        let mut descriptor = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(native_error());
        }
        if descriptor.is_null() {
            return Err(AuthError::Protection);
        }
        Ok(LocalAllocation(descriptor))
    }

    fn validate_acl(bytes: &[u8], user: &[u8]) -> Result<(), AuthError> {
        if bytes.len() < 8
            || !matches!(bytes[0], 2 | 4)
            || bytes[1] != 0
            || u16::from_le_bytes([bytes[2], bytes[3]]) as usize != bytes.len()
            || bytes[6..8] != [0, 0]
        {
            return Err(AuthError::Protection);
        }
        let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        if count != 3 {
            return Err(AuthError::Protection);
        }
        let allowed = [user, SYSTEM_SID, ADMINISTRATORS_SID];
        let mut seen = [false; 3];
        let mut offset = 8;
        for _ in 0..count {
            let head = bytes.get(offset..offset + 8).ok_or(AuthError::Protection)?;
            let len = u16::from_le_bytes([head[2], head[3]]) as usize;
            if head[0] != 0 || head[1] != 0 || len < 16 || len % 4 != 0 {
                return Err(AuthError::Protection);
            }
            let ace = bytes
                .get(offset..offset + len)
                .ok_or(AuthError::Protection)?;
            let mask = u32::from_le_bytes(ace[4..8].try_into().map_err(|_| AuthError::Protection)?);
            let sid = &ace[8..];
            if sid[0] != 1
                || sid[1] > 15
                || sid.len() != 8 + 4 * sid[1] as usize
                || mask != FILE_ALL_ACCESS
            {
                return Err(AuthError::Protection);
            }
            let index = allowed
                .iter()
                .position(|candidate| *candidate == sid)
                .ok_or(AuthError::Protection)?;
            if seen[index] {
                return Err(AuthError::Protection);
            }
            seen[index] = true;
            offset += len;
        }
        if !seen.iter().all(|value| *value) || bytes[offset..].iter().any(|byte| *byte != 0) {
            return Err(AuthError::Protection);
        }
        Ok(())
    }

    fn private_security(handle: HANDLE, user: &[u8]) -> Result<(), AuthError> {
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut raw = null_mut();
        let result = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut raw,
            )
        };
        let descriptor = LocalAllocation(raw);
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result as i32).into());
        }
        if descriptor.0.is_null()
            || dacl.is_null()
            || unsafe { IsValidSecurityDescriptor(raw) } == 0
        {
            return Err(AuthError::Protection);
        }
        let size = unsafe { GetSecurityDescriptorLength(raw) } as usize;
        if !(20..=MAX_SECURITY_BYTES).contains(&size) {
            return Err(AuthError::Protection);
        }
        let mut control = 0;
        let mut revision = 0;
        if unsafe { GetSecurityDescriptorControl(raw, &mut control, &mut revision) } == 0 {
            return Err(native_error());
        }
        if revision != 1 || control & 0x9004 != 0x9004 || control & 0x0500 != 0 {
            return Err(AuthError::Protection);
        }
        if unsafe { sid_bytes(raw as usize, size, owner) }? != user {
            return Err(AuthError::Protection);
        }
        bounded(raw as usize, size, dacl as usize, 8)?;
        let header = unsafe { std::slice::from_raw_parts(dacl.cast::<u8>(), 8) };
        let acl_size = u16::from_le_bytes([header[2], header[3]]) as usize;
        bounded(raw as usize, size, dacl as usize, acl_size)?;
        validate_acl(
            unsafe { std::slice::from_raw_parts(dacl.cast::<u8>(), acl_size) },
            user,
        )
    }

    #[derive(Clone)]
    pub struct WindowsPrivateRoot {
        directory: WindowsPinnedDirectory,
        user: Vec<u8>,
    }
    impl WindowsPrivateRoot {
        pub fn create_new(path: &Path) -> Result<Self, AuthError> {
            // Validate the original final component before Path normalizes a trailing ".".
            let text = path.as_os_str().to_str().ok_or(AuthError::Protection)?;
            let raw_name = text.rsplit('\\').next().ok_or(AuthError::Protection)?;
            component(OsStr::new(raw_name))?;
            let name = path.file_name().ok_or(AuthError::Protection)?;
            if name != OsStr::new(raw_name) {
                return Err(AuthError::Protection);
            }
            component(name)?;
            let parent =
                WindowsPinnedDirectory::open_existing(path.parent().ok_or(AuthError::Protection)?)?;
            Self::create_child(&parent, name)
        }

        pub fn create_child(
            parent: &WindowsPinnedDirectory,
            name: &OsStr,
        ) -> Result<Self, AuthError> {
            let user = current_sid()?;
            let descriptor = private_descriptor(&user)?;
            let handle = relative(
                parent,
                name,
                true,
                true,
                PRIVATE_DIRECTORY_ACCESS,
                descriptor.0,
            )?;
            let directory = parent.from_child(handle)?;
            let result = Self { directory, user };
            result.revalidate()?;
            Ok(result)
        }

        pub fn open_existing(path: &Path) -> Result<Self, AuthError> {
            let result = Self {
                directory: WindowsPinnedDirectory::open_existing_access(path, true)?,
                user: current_sid()?,
            };
            result.revalidate()?;
            Ok(result)
        }

        pub fn directory(&self) -> &WindowsPinnedDirectory {
            &self.directory
        }
        pub fn identity(&self) -> WindowsFileIdentity {
            self.directory.identity()
        }

        pub fn revalidate(&self) -> Result<(), AuthError> {
            if current_sid()? != self.user {
                return Err(AuthError::Protection);
            }
            self.directory.revalidate()?;
            private_security(self.directory.as_raw_handle(), &self.user)
        }

        pub fn create_file(&self, name: &OsStr) -> Result<WindowsPinnedFile, AuthError> {
            self.revalidate()?;
            let descriptor = private_descriptor(&self.user)?;
            let mut file = WindowsPinnedFile::new(
                &self.directory,
                relative(
                    &self.directory,
                    name,
                    false,
                    true,
                    STAGED_ACCESS,
                    descriptor.0,
                )?,
            )?;
            private_security(file.handle.as_raw_handle(), &self.user)?;
            self.revalidate()?;
            file.private_guard = Some((self.directory.clone(), self.user.clone()));
            file.revalidate()?;
            Ok(file)
        }

        pub fn open_file(&self, name: &OsStr) -> Result<WindowsPinnedFile, AuthError> {
            self.revalidate()?;
            let mut file = self.directory.open_file(name)?;
            private_security(file.handle.as_raw_handle(), &self.user)?;
            self.revalidate()?;
            file.private_guard = Some((self.directory.clone(), self.user.clone()));
            file.revalidate()?;
            Ok(file)
        }

        pub fn open_file_for_rename(&self, name: &OsStr) -> Result<WindowsPinnedFile, AuthError> {
            self.revalidate()?;
            let mut file = WindowsPinnedFile::new(
                &self.directory,
                relative(
                    &self.directory,
                    name,
                    false,
                    false,
                    STAGED_ACCESS,
                    null_mut(),
                )?,
            )?;
            file.private_guard = Some((self.directory.clone(), self.user.clone()));
            file.revalidate()?;
            Ok(file)
        }
    }

    pub struct WindowsPinnedFile {
        handle: OwnedHandle,
        identity: WindowsFileIdentity,
        parent: WindowsPinnedDirectory,
        volume_guid: Vec<u16>,
        private_guard: Option<(WindowsPinnedDirectory, Vec<u8>)>,
    }
    impl WindowsPinnedFile {
        fn new(parent: &WindowsPinnedDirectory, handle: OwnedHandle) -> Result<Self, AuthError> {
            let id = identity(handle.as_raw_handle(), false)?;
            let guid = final_guid(handle.as_raw_handle())?;
            if !id.same_volume(parent.identity()) || guid != parent.0.volume_guid {
                return Err(AuthError::Protection);
            }
            let file = Self {
                handle,
                identity: id,
                parent: parent.clone(),
                volume_guid: guid,
                private_guard: None,
            };
            file.revalidate()?;
            Ok(file)
        }

        pub fn identity(&self) -> WindowsFileIdentity {
            self.identity
        }
        pub fn revalidate(&self) -> Result<(), AuthError> {
            self.parent.revalidate()?;
            if let Some((root, user)) = &self.private_guard {
                if current_sid()? != *user {
                    return Err(AuthError::Protection);
                }
                root.revalidate()?;
                private_security(root.as_raw_handle(), user)?;
                private_security(self.handle.as_raw_handle(), user)?;
            }
            if identity(self.handle.as_raw_handle(), false)? != self.identity
                || final_guid(self.handle.as_raw_handle())? != self.volume_guid
            {
                return Err(AuthError::Protection);
            }
            Ok(())
        }

        /// Consume the file capability; the lock cannot be cloned or detached.
        pub fn try_lock_shared(self) -> Result<WindowsFileLock, AuthError> {
            self.try_lock(false)
        }

        pub fn try_lock_exclusive(self) -> Result<WindowsFileLock, AuthError> {
            self.try_lock(true)
        }

        fn try_lock(self, exclusive: bool) -> Result<WindowsFileLock, AuthError> {
            self.revalidate()?;
            let mut offset: OVERLAPPED = unsafe { zeroed() };
            let flags = LOCKFILE_FAIL_IMMEDIATELY
                | if exclusive {
                    LOCKFILE_EXCLUSIVE_LOCK
                } else {
                    0
                };
            // All acquisitions cover byte zero, including an empty lease file.
            if unsafe { LockFileEx(self.handle.as_raw_handle(), flags, 0, 1, 0, &mut offset) } == 0
            {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                    Err(AuthError::Busy)
                } else {
                    Err(AuthError::Io(error))
                };
            }
            // Own the successful lock before fallible post-acquisition checks.
            let guard = WindowsFileLock { file: self };
            guard.revalidate()?;
            Ok(guard)
        }

        pub fn read_bytes(&self, limit: usize) -> Result<Vec<u8>, AuthError> {
            self.revalidate()?;
            let mut file = File::from(self.handle.try_clone()?);
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            file.take(limit.checked_add(1).ok_or(AuthError::Protection)? as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err(AuthError::Protection);
            }
            self.revalidate()?;
            Ok(bytes)
        }

        pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), AuthError> {
            self.revalidate()?;
            let mut file = File::from(self.handle.try_clone()?);
            file.seek(SeekFrom::Start(0))?;
            file.write_all(bytes)?;
            file.set_len(bytes.len() as u64)?;
            self.revalidate()
        }

        pub fn flush(&self) -> Result<(), AuthError> {
            self.revalidate()?;
            if unsafe { FlushFileBuffers(self.handle.as_raw_handle()) } == 0 {
                return Err(native_error());
            }
            self.revalidate()
        }

        pub fn rename_no_clobber(
            &mut self,
            destination: &WindowsPinnedDirectory,
            name: &OsStr,
        ) -> Result<(), AuthError> {
            let name = component(name)?;
            self.revalidate()?;
            destination.revalidate()?;
            if !self.identity.same_volume(destination.identity())
                || self.volume_guid != destination.0.volume_guid
            {
                return Err(AuthError::Protection);
            }
            let size = size_of::<RenameInformation>() + (name.len() - 1) * 2;
            let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
            let information = buffer.as_mut_ptr().cast::<RenameInformation>();
            unsafe {
                (*information).replace_if_exists = 0;
                (*information).root_directory = destination.as_raw_handle();
                (*information).file_name_length = ((name.len() - 1) * 2) as u32;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    (*information).file_name.as_mut_ptr(),
                    name.len() - 1,
                );
            }
            let mut status = unsafe { zeroed() };
            let result = unsafe {
                NtSetInformationFile(
                    self.handle.as_raw_handle(),
                    &mut status,
                    buffer.as_ptr().cast(),
                    size as u32,
                    10,
                )
            };
            nt_result(result)?;
            self.parent.revalidate()?;
            self.parent = destination.clone();
            self.revalidate()?;
            // Reopen the destination relative to its retained pin and compare the full identity.
            // The source DELETE handle requires a matching share-delete on this read-only verifier.
            let mut name = name;
            let mut unicode = UNICODE_STRING {
                Length: ((name.len() - 1) * 2) as u16,
                MaximumLength: (name.len() * 2) as u16,
                Buffer: name.as_mut_ptr(),
            };
            let mut attributes = ObjectAttributes {
                length: size_of::<ObjectAttributes>() as u32,
                root_directory: destination.as_raw_handle(),
                object_name: &mut unicode,
                attributes: 0x1040,
                security_descriptor: null_mut(),
                security_quality_of_service: null_mut(),
            };
            let mut handle = null_mut();
            let result = unsafe {
                NtCreateFile(
                    &mut handle,
                    READ_ACCESS,
                    &mut attributes,
                    &mut status,
                    null(),
                    0,
                    7,
                    FILE_OPEN,
                    OPEN_NO_REPARSE_SYNC | NON_DIRECTORY,
                    null(),
                    0,
                )
            };
            nt_result(result)?;
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err(AuthError::Protection);
            }
            let verifier = unsafe { OwnedHandle::from_raw_handle(handle) };
            if status.Information != 1
                || identity(verifier.as_raw_handle(), false)? != self.identity
            {
                return Err(AuthError::Protection);
            }
            destination.revalidate()?;
            self.revalidate()
        }
    }

    impl WindowsPrivateRoot {
        pub fn rename_child_no_clobber(
            &self,
            source: &OsStr,
            target: &OsStr,
        ) -> Result<(), AuthError> {
            component(source)?;
            let name = component(target)?;
            self.revalidate()?;
            let handle = relative(
                &self.directory,
                source,
                true,
                false,
                PRIVATE_DIRECTORY_ACCESS | 0x0001_0000,
                null_mut(),
            )?;
            let before = identity(handle.as_raw_handle(), true)?;
            if !before.same_volume(self.identity())
                || final_guid(handle.as_raw_handle())? != self.directory.0.volume_guid
            {
                return Err(AuthError::Protection);
            }
            private_security(handle.as_raw_handle(), &self.user)?;
            let size = size_of::<RenameInformation>() + (name.len() - 1) * 2;
            let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
            let information = buffer.as_mut_ptr().cast::<RenameInformation>();
            unsafe {
                (*information).replace_if_exists = 0;
                (*information).root_directory = self.directory.as_raw_handle();
                (*information).file_name_length = ((name.len() - 1) * 2) as u32;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    (*information).file_name.as_mut_ptr(),
                    name.len() - 1,
                );
            }
            let mut status = unsafe { zeroed() };
            nt_result(unsafe {
                NtSetInformationFile(
                    handle.as_raw_handle(),
                    &mut status,
                    buffer.as_ptr().cast(),
                    size as u32,
                    10,
                )
            })?;
            self.revalidate()?;
            if identity(handle.as_raw_handle(), true)? != before {
                return Err(AuthError::Protection);
            }
            private_security(handle.as_raw_handle(), &self.user)?;
            // A verifier shares DELETE only while the owned source DELETE handle lives.
            let verifier = relative_shared(
                &self.directory,
                target,
                true,
                false,
                PRIVATE_DIRECTORY_ACCESS,
                null_mut(),
                7,
            )?;
            if identity(verifier.as_raw_handle(), true)? != before
                || final_guid(verifier.as_raw_handle())? != self.directory.0.volume_guid
            {
                return Err(AuthError::Protection);
            }
            private_security(verifier.as_raw_handle(), &self.user)?;
            self.revalidate()
        }

        /// Mutable SQLite files alone share READ/WRITE; their names remain pinned.
        pub fn open_sqlite_file(
            &self,
            name: &OsStr,
            writable: bool,
            create: bool,
            exclusive: bool,
        ) -> Result<WindowsSqliteFile, AuthError> {
            self.revalidate()?;
            component(name)?;
            if exclusive && (!create || !writable) {
                return Err(AuthError::Protection);
            }
            let access = READ_ACCESS | if writable { 0x102 } else { 0 };
            let open = || {
                relative_shared(
                    &self.directory,
                    name,
                    false,
                    false,
                    access,
                    null_mut(),
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )
            };
            let handle = if exclusive {
                let descriptor = private_descriptor(&self.user)?;
                relative_shared(
                    &self.directory,
                    name,
                    false,
                    true,
                    access,
                    descriptor.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )?
            } else {
                match open() {
                    Ok(handle) => handle,
                    Err(AuthError::Io(error))
                        if create && writable && error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        let descriptor = private_descriptor(&self.user)?;
                        match relative_shared(
                            &self.directory,
                            name,
                            false,
                            true,
                            access,
                            descriptor.0,
                            FILE_SHARE_READ | FILE_SHARE_WRITE,
                        ) {
                            Ok(handle) => handle,
                            Err(AuthError::Io(error))
                                if error.kind() == std::io::ErrorKind::AlreadyExists =>
                            {
                                open()?
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Err(error) => return Err(error),
                }
            };
            let mut file = WindowsPinnedFile::new(&self.directory, handle)?;
            file.private_guard = Some((self.directory.clone(), self.user.clone()));
            let result = WindowsSqliteFile {
                pin: Arc::new(SqlitePin {
                    file,
                    name: name.to_owned(),
                    io: Mutex::new(()),
                }),
            };
            result.revalidate()?;
            self.revalidate()?;
            Ok(result)
        }

        /// Delete a closed sidecar by its observed full identity, never by pathname.
        pub fn delete_sqlite_sidecar(
            &self,
            name: &OsStr,
            expected: WindowsFileIdentity,
        ) -> Result<bool, AuthError> {
            self.revalidate()?;
            let handle = match relative_shared(
                &self.directory,
                name,
                false,
                false,
                READ_ACCESS | 0x0001_0000,
                null_mut(),
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            ) {
                Ok(handle) => handle,
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(false);
                }
                Err(error) => return Err(error),
            };
            let mut file = WindowsPinnedFile::new(&self.directory, handle)?;
            file.private_guard = Some((self.directory.clone(), self.user.clone()));
            file.revalidate()?;
            if file.identity() != expected {
                return Err(AuthError::Protection);
            }
            let delete: u8 = 1;
            let mut status = unsafe { zeroed() };
            nt_result(unsafe {
                NtSetInformationFile(
                    file.handle.as_raw_handle(),
                    &mut status,
                    (&delete as *const u8).cast(),
                    1,
                    13,
                )
            })?;
            self.revalidate()?;
            drop(file); // The qualified DELETE handle closes before absence read-back.
            match relative_shared(
                &self.directory,
                name,
                false,
                false,
                READ_ACCESS,
                null_mut(),
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            ) {
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(AuthError::Protection),
            }
            self.revalidate()?;
            Ok(true)
        }
    }

    // Family capabilities deliberately use their own sharing contract. They do
    // not export a File/handle or borrow SQLite's callback lifetime rules.
    struct FamilyProtection {
        root: WindowsPrivateRoot,
        parent: Option<Arc<FamilyProtection>>,
    }
    impl FamilyProtection {
        fn revalidate(&self) -> Result<(), AuthError> {
            // This retained directory chain includes every private family root.
            self.root.directory.revalidate()?;
            let user = current_sid()?;
            let mut guard = Some(self);
            while let Some(current) = guard {
                if user != current.root.user {
                    return Err(AuthError::Protection);
                }
                private_security(current.root.directory.as_raw_handle(), &user)?;
                guard = current.parent.as_deref();
            }
            Ok(())
        }
    }
    pub struct WindowsFamilyDirectory {
        guard: Arc<FamilyProtection>,
        io: OwnedHandle,
        census: Mutex<()>,
    }
    pub struct WindowsFamilyEntry {
        pub name: OsString,
        pub child: Option<WindowsFamilyDirectory>,
    }

    fn legacy_identity(handle: HANDLE) -> Result<(u64, u64, u32), AuthError> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return Err(native_error());
        }
        Ok((
            u64::from(info.dwVolumeSerialNumber),
            u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
            info.nNumberOfLinks,
        ))
    }

    impl std::fmt::Debug for WindowsFamilyDirectory {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WindowsFamilyDirectory")
                .finish_non_exhaustive()
        }
    }
    impl WindowsFamilyDirectory {
        pub fn open_existing(path: &Path) -> Result<Self, AuthError> {
            let root = WindowsPrivateRoot::open_existing(path)?;
            let parent = root
                .directory
                .0
                .parent
                .as_ref()
                .ok_or(AuthError::Protection)?;
            let name = path.file_name().ok_or(AuthError::Protection)?;
            Self::qualified(root.clone(), parent, name, None)
        }
        fn qualified(
            root: WindowsPrivateRoot,
            parent: &WindowsPinnedDirectory,
            name: &OsStr,
            protection_parent: Option<Arc<FamilyProtection>>,
        ) -> Result<Self, AuthError> {
            if root
                .directory
                .0
                .parent
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(&actual.0, &parent.0))
                || protection_parent
                    .as_ref()
                    .is_some_and(|guard| !Arc::ptr_eq(&guard.root.directory.0, &parent.0))
            {
                return Err(AuthError::Protection);
            }
            // Real directory write/list access is required for flush/census.
            let io = relative(
                parent,
                name,
                true,
                false,
                PRIVATE_DIRECTORY_ACCESS | 0x0012_0117,
                null_mut(),
            )?;
            let value = Self {
                guard: Arc::new(FamilyProtection {
                    root,
                    parent: protection_parent,
                }),
                io,
                census: Mutex::new(()),
            };
            value.revalidate()?;
            Ok(value)
        }
        pub fn identity(&self) -> WindowsFileIdentity {
            self.guard.root.identity()
        }
        pub fn legacy_identity(&self) -> Result<(u64, u64), AuthError> {
            self.revalidate()?;
            let (volume, index, _) = legacy_identity(self.io.as_raw_handle())?;
            self.revalidate()?;
            Ok((volume, index))
        }
        pub fn revalidate(&self) -> Result<(), AuthError> {
            self.guard.revalidate()?;
            if identity(self.io.as_raw_handle(), true)? != self.identity()
                || final_guid(self.io.as_raw_handle())? != self.guard.root.directory.0.volume_guid
            {
                return Err(AuthError::Protection);
            }
            private_security(self.io.as_raw_handle(), &self.guard.root.user)
        }
        pub fn child(&self, name: &OsStr, create: bool) -> Result<Self, AuthError> {
            self.revalidate()?;
            let root = if create {
                WindowsPrivateRoot::create_child(&self.guard.root.directory, name)?
            } else {
                let handle = relative(
                    &self.guard.root.directory,
                    name,
                    true,
                    false,
                    PRIVATE_DIRECTORY_ACCESS,
                    null_mut(),
                )?;
                WindowsPrivateRoot {
                    directory: self.guard.root.directory.from_child(handle)?,
                    user: self.guard.root.user.clone(),
                }
            };
            root.revalidate()?;
            let child = Self::qualified(
                root,
                &self.guard.root.directory,
                name,
                Some(self.guard.clone()),
            )?;
            self.revalidate()?;
            Ok(child)
        }
        fn pinned_file(
            &self,
            name: &OsStr,
            write: bool,
            create: bool,
            deleting: bool,
            share_delete: bool,
        ) -> Result<WindowsPinnedFile, AuthError> {
            self.revalidate()?;
            let descriptor = if create {
                Some(private_descriptor(&self.guard.root.user)?)
            } else {
                None
            };
            let access =
                READ_ACCESS | if write { 0x116 } else { 0 } | if deleting { 0x10000 } else { 0 };
            let mut file = WindowsPinnedFile::new(
                &self.guard.root.directory,
                relative_shared(
                    &self.guard.root.directory,
                    name,
                    false,
                    create,
                    access,
                    descriptor.as_ref().map_or(null_mut(), |value| value.0),
                    FILE_SHARE_READ | FILE_SHARE_WRITE | if share_delete { 4 } else { 0 },
                )?,
            )?;
            file.private_guard = Some((
                self.guard.root.directory.clone(),
                self.guard.root.user.clone(),
            ));
            file.revalidate()?;
            self.revalidate()?;
            Ok(file)
        }
        pub fn open_file(
            &self,
            name: &OsStr,
            write: bool,
            create: bool,
        ) -> Result<WindowsFamilyFile, AuthError> {
            let file = self.pinned_file(name, write, create, false, false)?;
            self.family_file(name, file)
        }
        /// A read-only snapshot of a mutable publication target. Only this
        /// admission permits DELETE sharing; ordinary immutable readers keep
        /// blocking replacement until close.
        pub fn open_mutable_file(&self, name: &OsStr) -> Result<WindowsFamilyFile, AuthError> {
            let file = self.pinned_file(name, false, false, false, true)?;
            self.family_file(name, file)
        }
        fn family_file(
            &self,
            name: &OsStr,
            file: WindowsPinnedFile,
        ) -> Result<WindowsFamilyFile, AuthError> {
            if legacy_identity(file.handle.as_raw_handle())?.2 != 1 {
                return Err(AuthError::Protection);
            }
            let io = File::from(file.handle.try_clone()?);
            let value = WindowsFamilyFile {
                io,
                pin: Arc::new(FamilyPin {
                    file,
                    name: name.to_owned(),
                    protection: self.guard.clone(),
                }),
            };
            value.validate()?;
            Ok(value)
        }
        pub fn sync_all(&self) -> Result<(), AuthError> {
            self.revalidate()?;
            let mut status = unsafe { zeroed() };
            nt_result(unsafe { NtFlushBuffersFile(self.io.as_raw_handle(), &mut status) })?;
            self.revalidate()
        }
        pub fn entries(&self, maximum: usize) -> Result<Vec<WindowsFamilyEntry>, AuthError> {
            if maximum > 65_536 {
                return Err(AuthError::Protection);
            }
            let _scan = self.census.lock().map_err(|_| AuthError::Protection)?;
            self.revalidate()?;
            let mut names = std::collections::BTreeMap::new();
            let mut restart = true;
            let mut batches = 0usize;
            loop {
                batches += 1;
                if batches > maximum + 3 {
                    return Err(AuthError::Protection);
                }
                let mut buffer = vec![0u64; 8192];
                let class = if restart {
                    FileIdBothDirectoryRestartInfo
                } else {
                    FileIdBothDirectoryInfo
                };
                if unsafe {
                    GetFileInformationByHandleEx(
                        self.io.as_raw_handle(),
                        class,
                        buffer.as_mut_ptr().cast(),
                        (buffer.len() * 8) as u32,
                    )
                } == 0
                {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(18) {
                        break;
                    } // ERROR_NO_MORE_FILES
                    return Err(error.into());
                }
                restart = false;
                let bytes = unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * 8)
                };
                let mut offset = 0usize;
                loop {
                    let header = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
                    if offset % 8 != 0
                        || offset
                            .checked_add(header)
                            .is_none_or(|end| end > bytes.len())
                    {
                        return Err(AuthError::Protection);
                    }
                    let info =
                        unsafe { &*bytes.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>() };
                    let length = info.FileNameLength as usize;
                    let end = offset
                        .checked_add(header)
                        .and_then(|start| start.checked_add(length))
                        .ok_or(AuthError::Protection)?;
                    let next = info.NextEntryOffset as usize;
                    if length == 0
                        || length % 2 != 0
                        || length > 510
                        || end > bytes.len()
                        || (next != 0
                            && (next % 8 != 0
                                || next < header + length
                                || offset + next >= bytes.len()))
                    {
                        return Err(AuthError::Protection);
                    }
                    let name = String::from_utf16(unsafe {
                        std::slice::from_raw_parts(
                            bytes.as_ptr().add(offset + header).cast::<u16>(),
                            length / 2,
                        )
                    })
                    .map_err(|_| AuthError::Protection)?;
                    if name != "." && name != ".." {
                        component(OsStr::new(&name))?;
                        if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                            return Err(AuthError::Protection);
                        }
                        let directory = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                        // Each relevant entry is qualified again relative to the retained parent.
                        let child = if directory {
                            Some(self.child(OsStr::new(&name), false)?)
                        } else {
                            None
                        };
                        let actual = if let Some(child) = &child {
                            child.legacy_identity()?.1
                        } else {
                            let file = self.open_file(OsStr::new(&name), false, false)?;
                            legacy_identity(file.io.as_raw_handle())?.1
                        };
                        if actual != info.FileId as u64
                            || names.insert(name, child).is_some()
                            || names.len() > maximum
                        {
                            return Err(AuthError::Protection);
                        }
                    }
                    if next == 0 {
                        break;
                    }
                    offset += next;
                }
                self.revalidate()?;
            }
            self.revalidate()?;
            Ok(names
                .into_iter()
                .map(|(name, child)| WindowsFamilyEntry {
                    name: name.into(),
                    child,
                })
                .collect())
        }
        pub fn link_no_replace(&self, source: &OsStr, target: &OsStr) -> Result<bool, AuthError> {
            component(target)?;
            let file = self.pinned_file(source, false, false, false, false)?;
            if legacy_identity(file.handle.as_raw_handle())?.2 != 1 {
                return Err(AuthError::Protection);
            }
            match self.open_file(target, false, false) {
                Ok(_) => return Ok(false),
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            match self.set_name(&file, target, 11, 0) {
                // FileLinkInformation: hard link, no replacement.
                Ok(()) => {}
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.open_file(target, false, false)?;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
            let linked = self.pinned_file(target, false, false, false, false)?;
            if linked.identity != file.identity
                || legacy_identity(file.handle.as_raw_handle())?.2 != 2
            {
                return Err(AuthError::Protection);
            }
            self.revalidate()?;
            Ok(true)
        }
        fn set_name(
            &self,
            file: &WindowsPinnedFile,
            target: &OsStr,
            class: i32,
            flags: u32,
        ) -> Result<(), AuthError> {
            #[repr(C)]
            struct NameInformation {
                flags: u32,
                root_directory: HANDLE,
                file_name_length: u32,
                file_name: [u16; 1],
            }
            let name = component(target)?;
            file.revalidate()?;
            self.revalidate()?;
            let offset = std::mem::offset_of!(NameInformation, file_name);
            let size = offset + (name.len() - 1) * 2;
            let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
            let info = buffer.as_mut_ptr().cast::<NameInformation>();
            unsafe {
                (*info).flags = flags;
                (*info).root_directory = self.guard.root.directory.as_raw_handle();
                (*info).file_name_length = ((name.len() - 1) * 2) as u32;
                std::ptr::copy_nonoverlapping(
                    name.as_ptr(),
                    (*info).file_name.as_mut_ptr(),
                    name.len() - 1,
                );
            }
            let mut status = unsafe { zeroed() };
            nt_result(unsafe {
                NtSetInformationFile(
                    file.handle.as_raw_handle(),
                    &mut status,
                    buffer.as_ptr().cast(),
                    size as u32,
                    class,
                )
            })?;
            self.revalidate()?;
            file.revalidate()
        }
        pub fn replace(&self, source: &OsStr, target: &OsStr) -> Result<(), AuthError> {
            if source == target {
                return Err(AuthError::Protection);
            }
            let source_file = self.pinned_file(source, false, false, true, true)?;
            if legacy_identity(source_file.handle.as_raw_handle())?.2 != 1 {
                return Err(AuthError::Protection);
            }
            let target_file = match self.open_mutable_file(target) {
                Ok(file) => Some(file),
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            // Existing owner locks serialize cooperating publishers. Keep the
            // old target pinned through POSIX replacement, never close/reopen it.
            self.set_name(
                &source_file,
                target,
                65,
                if target_file.is_some() { 3 } else { 0 },
            )?;
            let target_file = self.pinned_file(target, false, false, false, true)?;
            if target_file.identity != source_file.identity {
                return Err(AuthError::Protection);
            }
            match self.open_file(source, false, false) {
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(AuthError::Protection),
            }
            self.revalidate()
        }
        pub fn unlink_file(&self, name: &OsStr) -> Result<(), AuthError> {
            let file = self.pinned_file(name, false, false, true, true)?;
            let links = legacy_identity(file.handle.as_raw_handle())?.2;
            if links != 1 && links != 2 {
                return Err(AuthError::Protection);
            }
            // A two-link staging object must have its other qualified name in
            // this private directory, rather than an unexplained outside link.
            if links == 2 {
                self.require_local_link(&file, name)?;
            }
            let expected = file.identity;
            file.revalidate()?;
            if identity(file.handle.as_raw_handle(), false)? != expected {
                return Err(AuthError::Protection);
            }
            let delete: u8 = 1;
            let mut status = unsafe { zeroed() };
            nt_result(unsafe {
                NtSetInformationFile(
                    file.handle.as_raw_handle(),
                    &mut status,
                    (&delete as *const u8).cast(),
                    1,
                    13,
                )
            })?;
            drop(file);
            match self.open_file(name, false, false) {
                Err(AuthError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.revalidate()
                }
                _ => Err(AuthError::Protection),
            }
        }
        fn require_local_link(
            &self,
            file: &WindowsPinnedFile,
            name: &OsStr,
        ) -> Result<(), AuthError> {
            // Enumeration for this mutation may observe the two-link object;
            // the dedicated census below validates names/IDs without opening it.
            let names = self.link_names(file)?;
            if names.len() != 2 || !names.iter().any(|candidate| candidate == name) {
                return Err(AuthError::Protection);
            }
            Ok(())
        }
        fn link_names(&self, file: &WindowsPinnedFile) -> Result<Vec<OsString>, AuthError> {
            // FILE_LINKS_INFORMATION returns the hard-link census through the
            // qualified handle. Unsupported kernels fail closed.
            #[repr(C)]
            struct Links {
                bytes: u32,
                count: u32,
            }
            #[repr(C)]
            struct Link {
                next: u32,
                parent_id: i64,
                length: u32,
                name: [u16; 1],
            }
            #[link(name = "ntdll")]
            unsafe extern "system" {
                fn NtQueryInformationFile(
                    handle: HANDLE,
                    status: *mut IO_STATUS_BLOCK,
                    info: *mut c_void,
                    length: u32,
                    class: i32,
                ) -> i32;
            }
            let mut buffer = vec![0u64; 8192];
            let mut status: IO_STATUS_BLOCK = unsafe { zeroed() };
            file.revalidate()?;
            nt_result(unsafe {
                NtQueryInformationFile(
                    file.handle.as_raw_handle(),
                    &mut status,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                    46,
                )
            })?;
            let length = status.Information;
            if length < size_of::<Links>() || length > buffer.len() * 8 {
                return Err(AuthError::Protection);
            }
            let head = unsafe { &*buffer.as_ptr().cast::<Links>() };
            if head.count != 2 || head.bytes as usize != length {
                return Err(AuthError::Protection);
            }
            let parent_id = self.legacy_identity()?.1;
            let mut offset = 8usize;
            let mut names = Vec::new();
            for index in 0..2 {
                let prefix = std::mem::offset_of!(Link, name);
                if offset % 8 != 0 || offset + prefix > length {
                    return Err(AuthError::Protection);
                }
                let entry = unsafe { &*buffer.as_ptr().cast::<u8>().add(offset).cast::<Link>() };
                // FileNameLength is a UTF-16 character count (unlike the
                // directory census API, which reports a byte count).
                let characters = entry.length as usize;
                let bytes = characters.checked_mul(2).ok_or(AuthError::Protection)?;
                if characters == 0
                    || characters > 255
                    || offset + prefix + bytes > length
                    || entry.parent_id as u64 != parent_id
                {
                    return Err(AuthError::Protection);
                }
                let name = String::from_utf16(unsafe {
                    std::slice::from_raw_parts(entry.name.as_ptr(), characters)
                })
                .map_err(|_| AuthError::Protection)?;
                component(OsStr::new(&name))?;
                let candidate = self.pinned_file(OsStr::new(&name), false, false, false, true)?;
                if candidate.identity != file.identity {
                    return Err(AuthError::Protection);
                }
                names.push(OsString::from(name));
                if index == 0 {
                    let next = entry.next as usize;
                    if next % 8 != 0 || next < prefix + bytes {
                        return Err(AuthError::Protection);
                    }
                    offset = offset.checked_add(next).ok_or(AuthError::Protection)?;
                } else if entry.next != 0 {
                    return Err(AuthError::Protection);
                }
            }
            file.revalidate()?;
            Ok(names)
        }
        pub fn rename_child_no_clobber(
            &self,
            source: &OsStr,
            target: &OsStr,
        ) -> Result<(), AuthError> {
            self.revalidate()?;
            self.guard.root.rename_child_no_clobber(source, target)?;
            self.revalidate()
        }
    }

    struct FamilyPin {
        file: WindowsPinnedFile,
        name: OsString,
        protection: Arc<FamilyProtection>,
    }
    pub struct WindowsFamilyFile {
        io: File,
        pin: Arc<FamilyPin>,
    }
    pub struct WindowsFamilyMetadata {
        metadata: std::fs::Metadata,
        _pin: Arc<FamilyPin>,
    }
    impl WindowsFamilyMetadata {
        pub fn len(&self) -> u64 {
            self.metadata.len()
        }
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }
    fn family_io_error(error: AuthError) -> std::io::Error {
        match error {
            AuthError::Io(error) => error,
            _ => std::io::Error::other("qualified Windows family binding changed"),
        }
    }
    impl std::fmt::Debug for WindowsFamilyFile {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WindowsFamilyFile").finish_non_exhaustive()
        }
    }
    impl WindowsFamilyFile {
        fn validate(&self) -> Result<(), AuthError> {
            self.pin.protection.revalidate()?;
            let root = &self.pin.protection.root;
            let file = &self.pin.file;
            let (private_root, user) = file.private_guard.as_ref().ok_or(AuthError::Protection)?;
            if !Arc::ptr_eq(&file.parent.0, &root.directory.0)
                || !Arc::ptr_eq(&private_root.0, &root.directory.0)
                || user != &root.user
            {
                return Err(AuthError::Protection);
            }
            private_security(file.handle.as_raw_handle(), user)?;
            if identity(file.handle.as_raw_handle(), false)? != file.identity
                || final_guid(file.handle.as_raw_handle())? != file.volume_guid
                || identity(self.io.as_raw_handle(), false)? != file.identity
            {
                return Err(AuthError::Protection);
            }
            Ok(())
        }
        pub fn identity(&self) -> Result<WindowsFileIdentity, std::io::Error> {
            self.validate().map_err(family_io_error)?;
            Ok(self.pin.file.identity)
        }
        pub fn legacy_identity(&self) -> std::io::Result<(u64, u64)> {
            self.validate().map_err(family_io_error)?;
            let (volume, index, _) =
                legacy_identity(self.io.as_raw_handle()).map_err(family_io_error)?;
            self.validate().map_err(family_io_error)?;
            Ok((volume, index))
        }
        pub fn version(&self) -> std::io::Result<(WindowsFileIdentity, u64, i64, i64)> {
            self.validate().map_err(family_io_error)?;
            let mut basic =
                unsafe { zeroed::<windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO>() };
            if unsafe {
                GetFileInformationByHandleEx(
                    self.io.as_raw_handle(),
                    windows_sys::Win32::Storage::FileSystem::FileBasicInfo,
                    (&mut basic as *mut windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO)
                        .cast(),
                    size_of::<windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO>() as u32,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let length = self.io.metadata()?.len();
            self.validate().map_err(family_io_error)?;
            Ok((
                self.pin.file.identity,
                length,
                basic.LastWriteTime,
                basic.ChangeTime,
            ))
        }
        pub fn metadata(&self) -> std::io::Result<WindowsFamilyMetadata> {
            self.validate().map_err(family_io_error)?;
            let metadata = self.io.metadata()?;
            self.validate().map_err(family_io_error)?;
            Ok(WindowsFamilyMetadata {
                metadata,
                _pin: self.pin.clone(),
            })
        }
        pub fn set_len(&self, length: u64) -> std::io::Result<()> {
            self.checked(|| self.io.set_len(length))
        }
        pub fn try_clone(&self) -> std::io::Result<Self> {
            self.checked(|| self.io.try_clone()).map(|io| Self {
                io,
                pin: self.pin.clone(),
            })
        }
        pub fn sync_all(&self) -> std::io::Result<()> {
            self.checked(|| {
                // A real writable handle, admitted relative to the same parent,
                // is required even when the reader's own handle is read-only.
                let pinned = &self.pin.file;
                let handle = relative_shared(
                    &pinned.parent,
                    &self.pin.name,
                    false,
                    false,
                    READ_ACCESS | 0x2,
                    null_mut(),
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                )
                .map_err(family_io_error)?;
                if identity(handle.as_raw_handle(), false).map_err(family_io_error)?
                    != pinned.identity
                {
                    return Err(family_io_error(AuthError::Protection));
                }
                if unsafe { FlushFileBuffers(handle.as_raw_handle()) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        }
        fn checked<T>(&self, action: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
            self.validate().map_err(family_io_error)?;
            let result = action()?;
            self.validate().map_err(family_io_error)?;
            Ok(result)
        }
        fn lock(&self, shared: bool) -> Result<(), std::fs::TryLockError> {
            self.validate()
                .map_err(family_io_error)
                .map_err(std::fs::TryLockError::Error)?;
            if shared {
                self.io.try_lock_shared()?;
            } else {
                self.io.try_lock()?;
            }
            if let Err(error) = self.validate() {
                let _ = self.io.unlock();
                return Err(std::fs::TryLockError::Error(family_io_error(error)));
            }
            Ok(())
        }
        pub fn try_lock(&self) -> Result<(), std::fs::TryLockError> {
            self.lock(false)
        }
        pub fn try_lock_shared(&self) -> Result<(), std::fs::TryLockError> {
            self.lock(true)
        }
        pub fn unlock(&self) -> std::io::Result<()> {
            let before = self.validate().map_err(family_io_error);
            let released = self.io.unlock();
            before?;
            released?;
            self.validate().map_err(family_io_error)
        }
    }
    impl Read for WindowsFamilyFile {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.validate().map_err(family_io_error)?;
            let n = self.io.read(bytes)?;
            self.validate().map_err(family_io_error)?;
            Ok(n)
        }
    }
    impl Write for WindowsFamilyFile {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.validate().map_err(family_io_error)?;
            let n = self.io.write(bytes)?;
            self.validate().map_err(family_io_error)?;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.validate().map_err(family_io_error)?;
            self.io.flush()?;
            self.validate().map_err(family_io_error)
        }
    }
    impl Seek for WindowsFamilyFile {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.validate().map_err(family_io_error)?;
            let n = self.io.seek(position)?;
            self.validate().map_err(family_io_error)?;
            Ok(n)
        }
    }

    struct SqlitePin {
        file: WindowsPinnedFile,
        name: OsString,
        io: Mutex<()>,
    }
    pub struct WindowsSqliteFile {
        pin: Arc<SqlitePin>,
    }

    impl WindowsSqliteFile {
        pub fn identity(&self) -> WindowsFileIdentity {
            self.pin.file.identity()
        }

        pub fn revalidate(&self) -> Result<(), AuthError> {
            self.pin.file.revalidate()?;
            let named = relative_shared(
                &self.pin.file.parent,
                &self.pin.name,
                false,
                false,
                READ_ACCESS,
                null_mut(),
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            )?;
            let (_, user) = self
                .pin
                .file
                .private_guard
                .as_ref()
                .ok_or(AuthError::Protection)?;
            private_security(named.as_raw_handle(), user)?;
            if identity(named.as_raw_handle(), false)? != self.identity() {
                return Err(AuthError::Protection);
            }
            self.pin.file.revalidate()
        }

        pub fn read_at(&self, offset: u64, bytes: &mut [u8]) -> Result<usize, AuthError> {
            if offset
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > i64::MAX as u64)
                || bytes.len() > u32::MAX as usize
            {
                return Err(AuthError::Protection);
            }
            let _io = self.pin.io.lock().map_err(|_| AuthError::Protection)?;
            self.revalidate()?;
            if unsafe {
                SetFilePointerEx(
                    self.pin.file.handle.as_raw_handle(),
                    offset as i64,
                    null_mut(),
                    FILE_BEGIN,
                )
            } == 0
            {
                return Err(native_error());
            }
            let mut count = 0;
            if unsafe {
                ReadFile(
                    self.pin.file.handle.as_raw_handle(),
                    bytes.as_mut_ptr(),
                    bytes.len() as u32,
                    &mut count,
                    null_mut(),
                )
            } == 0
            {
                return Err(native_error());
            }
            self.revalidate()?;
            Ok(count as usize)
        }

        pub fn write_at(&self, offset: u64, bytes: &[u8]) -> Result<(), AuthError> {
            if offset
                .checked_add(bytes.len() as u64)
                .is_none_or(|end| end > i64::MAX as u64)
            {
                return Err(AuthError::Protection);
            }
            let _io = self.pin.io.lock().map_err(|_| AuthError::Protection)?;
            self.revalidate()?;
            if unsafe {
                SetFilePointerEx(
                    self.pin.file.handle.as_raw_handle(),
                    offset as i64,
                    null_mut(),
                    FILE_BEGIN,
                )
            } == 0
            {
                return Err(native_error());
            }
            let mut remaining = bytes;
            while !remaining.is_empty() {
                let length = remaining.len().min(u32::MAX as usize) as u32;
                let mut count = 0;
                if unsafe {
                    WriteFile(
                        self.pin.file.handle.as_raw_handle(),
                        remaining.as_ptr(),
                        length,
                        &mut count,
                        null_mut(),
                    )
                } == 0
                {
                    return Err(native_error());
                }
                if count == 0 || count > length {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into());
                }
                remaining = &remaining[count as usize..];
            }
            self.revalidate()
        }

        pub fn truncate(&self, size: u64) -> Result<(), AuthError> {
            let _io = self.pin.io.lock().map_err(|_| AuthError::Protection)?;
            self.revalidate()?;
            self.set_size(size)?;
            self.revalidate()
        }

        pub fn ensure_len(&self, size: u64) -> Result<(), AuthError> {
            let _io = self.pin.io.lock().map_err(|_| AuthError::Protection)?;
            self.revalidate()?;
            if self.file_size()? < size {
                self.set_size(size)?;
            }
            self.revalidate()
        }

        pub fn file_size(&self) -> Result<u64, AuthError> {
            self.revalidate()?;
            let mut size = 0;
            if unsafe { GetFileSizeEx(self.pin.file.handle.as_raw_handle(), &mut size) } == 0 {
                return Err(native_error());
            }
            if size < 0 {
                return Err(AuthError::Protection);
            }
            self.revalidate()?;
            Ok(size as u64)
        }

        fn set_size(&self, size: u64) -> Result<(), AuthError> {
            let end = FILE_END_OF_FILE_INFO {
                EndOfFile: i64::try_from(size).map_err(|_| AuthError::Protection)?,
            };
            if unsafe {
                SetFileInformationByHandle(
                    self.pin.file.handle.as_raw_handle(),
                    FileEndOfFileInfo,
                    (&end as *const FILE_END_OF_FILE_INFO).cast(),
                    size_of::<FILE_END_OF_FILE_INFO>() as u32,
                )
            } == 0
            {
                return Err(native_error());
            }
            Ok(())
        }

        pub fn flush(&self) -> Result<(), AuthError> {
            self.revalidate()?;
            self.pin.file.flush()?;
            self.revalidate()
        }

        pub fn sector_size(&self) -> Result<u32, AuthError> {
            self.revalidate()?;
            let mut guid = self.pin.file.volume_guid.clone();
            guid.push(0);
            let mut cluster = 0;
            let mut bytes = 0;
            let mut free = 0;
            let mut total = 0;
            if unsafe {
                GetDiskFreeSpaceW(
                    guid.as_ptr(),
                    &mut cluster,
                    &mut bytes,
                    &mut free,
                    &mut total,
                )
            } == 0
            {
                return Err(native_error());
            }
            if cluster == 0 || !(512..=65536).contains(&bytes) || !bytes.is_power_of_two() {
                return Err(AuthError::Protection);
            }
            self.revalidate()?;
            Ok(bytes)
        }

        pub fn try_lock_range(
            &self,
            offset: u64,
            length: u64,
            exclusive: bool,
        ) -> Result<WindowsSqliteRangeLock, AuthError> {
            if length == 0 || offset.checked_add(length).is_none() {
                return Err(AuthError::Protection);
            }
            self.revalidate()?;
            let mut at: OVERLAPPED = unsafe { zeroed() };
            at.Anonymous.Anonymous.Offset = offset as u32;
            at.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
            let flags = LOCKFILE_FAIL_IMMEDIATELY
                | if exclusive {
                    LOCKFILE_EXCLUSIVE_LOCK
                } else {
                    0
                };
            if unsafe {
                LockFileEx(
                    self.pin.file.handle.as_raw_handle(),
                    flags,
                    0,
                    length as u32,
                    (length >> 32) as u32,
                    &mut at,
                )
            } == 0
            {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                    Err(AuthError::Busy)
                } else {
                    Err(AuthError::Io(error))
                };
            }
            let guard = WindowsSqliteRangeLock {
                pin: Arc::clone(&self.pin),
                offset,
                length,
                active: true,
            };
            self.revalidate()?;
            Ok(guard)
        }
    }

    pub struct WindowsSqliteRangeLock {
        pin: Arc<SqlitePin>,
        offset: u64,
        length: u64,
        active: bool,
    }
    impl WindowsSqliteRangeLock {
        pub fn release(mut self) -> Result<(), AuthError> {
            self.unlock()
        }
        fn unlock(&mut self) -> Result<(), AuthError> {
            if !self.active {
                return Ok(());
            }
            let before = self.pin.file.revalidate();
            self.unlock_owned()?;
            before?;
            self.pin.file.revalidate()
        }
        fn unlock_owned(&mut self) -> Result<(), AuthError> {
            if !self.active {
                return Ok(());
            }
            let mut at: OVERLAPPED = unsafe { zeroed() };
            at.Anonymous.Anonymous.Offset = self.offset as u32;
            at.Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
            if unsafe {
                UnlockFileEx(
                    self.pin.file.handle.as_raw_handle(),
                    0,
                    self.length as u32,
                    (self.length >> 32) as u32,
                    &mut at,
                )
            } == 0
            {
                return Err(native_error());
            }
            self.active = false;
            Ok(())
        }
    }
    impl Drop for WindowsSqliteRangeLock {
        fn drop(&mut self) {
            let _ = self.unlock_owned();
        }
    }

    #[repr(C)]
    struct SqliteSystemInfo {
        processor: u32,
        page_size: u32,
        minimum: *mut c_void,
        maximum: *mut c_void,
        active_mask: usize,
        processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }
    const _: () =
        assert!(size_of::<SqliteSystemInfo>() == if size_of::<usize>() == 8 { 48 } else { 36 });
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "CreateFileMappingW"]
        fn sqlite_create_mapping(
            file: HANDLE,
            security: *const c_void,
            protect: u32,
            maximum_high: u32,
            maximum_low: u32,
            name: *const u16,
        ) -> HANDLE;
        #[link_name = "MapViewOfFile"]
        fn sqlite_map_view(
            mapping: HANDLE,
            access: u32,
            offset_high: u32,
            offset_low: u32,
            bytes: usize,
        ) -> *mut c_void;
        #[link_name = "UnmapViewOfFile"]
        fn sqlite_unmap_view(base: *const c_void) -> i32;
        #[link_name = "GetSystemInfo"]
        fn sqlite_system_info(info: *mut SqliteSystemInfo);
    }

    pub struct WindowsSqliteMapping {
        pin: Arc<SqlitePin>,
        _mapping: OwnedHandle,
        base: Option<*mut c_void>,
        data: *mut c_void,
    }
    impl WindowsSqliteFile {
        /// Only this qualified file can create a shared mapping; no handle import.
        pub fn map_shared(
            &self,
            offset: u64,
            length: usize,
        ) -> Result<WindowsSqliteMapping, AuthError> {
            self.revalidate()?;
            let end = offset
                .checked_add(length as u64)
                .ok_or(AuthError::Protection)?;
            if length == 0 || end > self.file_size()? {
                return Err(AuthError::Protection);
            }
            let mut info: SqliteSystemInfo = unsafe { zeroed() };
            unsafe { sqlite_system_info(&mut info) };
            let granularity = u64::from(info.allocation_granularity);
            if granularity == 0 || !granularity.is_power_of_two() || info.page_size == 0 {
                return Err(AuthError::Protection);
            }
            let aligned = offset - offset % granularity;
            let delta = usize::try_from(offset - aligned).map_err(|_| AuthError::Protection)?;
            let size = delta
                .checked_add(length)
                .filter(|n| *n <= isize::MAX as usize)
                .ok_or(AuthError::Protection)?;
            let raw = unsafe {
                sqlite_create_mapping(
                    self.pin.file.handle.as_raw_handle(),
                    null(),
                    4,
                    0,
                    0,
                    null(),
                )
            };
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                return Err(native_error());
            }
            let mapping = unsafe { OwnedHandle::from_raw_handle(raw) };
            let base = unsafe {
                sqlite_map_view(
                    mapping.as_raw_handle(),
                    2,
                    (aligned >> 32) as u32,
                    aligned as u32,
                    size,
                )
            };
            if base.is_null() {
                return Err(native_error());
            }
            let result = WindowsSqliteMapping {
                pin: Arc::clone(&self.pin),
                _mapping: mapping,
                base: Some(base),
                data: unsafe { base.cast::<u8>().add(delta).cast() },
            };
            self.revalidate()?;
            Ok(result)
        }
    }
    impl WindowsSqliteMapping {
        /// SQLite owns use of this pointer only while this mapping guard is held.
        pub fn as_mut_ptr(&self) -> *mut c_void {
            if self.base.is_some() {
                self.data
            } else {
                null_mut()
            }
        }
        pub fn revalidate(&self) -> Result<(), AuthError> {
            if self.base.is_none() {
                return Err(AuthError::Protection);
            }
            self.pin.file.revalidate()
        }
        pub fn unmap(&mut self) -> Result<(), AuthError> {
            if let Some(base) = self.base {
                self.revalidate()?;
                if unsafe { sqlite_unmap_view(base) } == 0 {
                    return Err(native_error());
                }
                self.base = None;
            }
            Ok(())
        }
    }
    impl Drop for WindowsSqliteMapping {
        fn drop(&mut self) {
            // Cleanup must still unmap if a protection check begins failing.
            if let Some(base) = self.base.take() {
                unsafe { sqlite_unmap_view(base) };
            }
        }
    }

    /// A live native byte-range lock, released before its pinned file closes.
    pub struct WindowsFileLock {
        file: WindowsPinnedFile,
    }

    impl WindowsFileLock {
        pub fn identity(&self) -> WindowsFileIdentity {
            self.file.identity()
        }
        pub fn revalidate(&self) -> Result<(), AuthError> {
            self.file.revalidate()
        }
    }

    impl std::fmt::Debug for WindowsFileLock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WindowsFileLock")
                .field("identity", &self.identity())
                .finish_non_exhaustive()
        }
    }

    impl Drop for WindowsFileLock {
        fn drop(&mut self) {
            let mut offset: OVERLAPPED = unsafe { zeroed() };
            // Closing the owned file also releases the lock if explicit unlock fails.
            unsafe { UnlockFileEx(self.file.handle.as_raw_handle(), 0, 1, 0, &mut offset) };
        }
    }
}

#[cfg(unix)]
const MAX_PATHS: usize = 4096;
#[cfg(unix)]
const MAX_IDENTITIES: usize = 65_536;
#[cfg(unix)]
const MAX_PATH_BYTES: usize = 16_384;
#[cfg(unix)]
const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Read at most one byte beyond the bound, including files that grow after stat.
#[cfg(unix)]
pub(crate) fn read_body(file: File) -> Result<Vec<u8>, AuthError> {
    let metadata = file.metadata()?;
    require_regular(&metadata)?;
    if metadata.len() > MAX_BODY_BYTES {
        return Err(AuthError::Protection);
    }
    let mut body = Vec::new();
    file.take(MAX_BODY_BYTES + 1).read_to_end(&mut body)?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(AuthError::Protection);
    }
    Ok(body)
}

#[cfg(unix)]
fn require_regular(metadata: &Metadata) -> Result<(), AuthError> {
    if !metadata.is_file() {
        return Err(AuthError::Protection);
    }
    Ok(())
}

/// Check before open, and again on the descriptor: a replaced FIFO must neither
/// block open nor be used as a body source/sink. Never truncate before fstat.
#[cfg(unix)]
pub(crate) fn open_regular(path: &Path, write: bool) -> Result<File, AuthError> {
    match std::fs::metadata(path) {
        Ok(metadata) => require_regular(&metadata)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AuthError::Io(e)),
    }
    open_regular_checked(path, write)
}

#[cfg(unix)]
fn open_regular_checked(path: &Path, write: bool) -> Result<File, AuthError> {
    let file = OpenOptions::new()
        .read(!write)
        .write(write)
        .create(write)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    require_regular(&file.metadata()?)?;
    Ok(file)
}

/// Resolve only existing ancestors; unresolved `..` is rejected, never folded
/// lexically across a missing directory or symlink.
#[cfg(unix)]
fn resolve_future_path(path: &Path) -> Result<PathBuf, AuthError> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for name in suffix.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(Component::Normal(name)) = ancestor.components().next_back() else {
                    return Err(AuthError::Protection);
                };
                suffix.push(name);
                ancestor = ancestor.parent().ok_or(AuthError::Protection)?;
            }
            Err(_) => return Err(AuthError::Protection),
        }
    }
}

#[cfg(unix)]
fn valid_registered_path(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= MAX_PATH_BYTES
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

#[cfg(unix)]
struct PreparedPath {
    operation: PathBuf,
    names: Vec<PathBuf>,
    metadata: Option<Metadata>,
}

#[cfg(unix)]
impl PreparedPath {
    fn new(path: &Path) -> Result<Self, AuthError> {
        let absolute = std::path::absolute(path).map_err(|_| AuthError::Protection)?;
        if absolute.as_os_str().len() > MAX_PATH_BYTES
            || absolute.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(AuthError::Protection);
        }
        let Some(Component::Normal(name)) = absolute.components().next_back() else {
            return Err(AuthError::Protection);
        };
        // Preserve rename/unlink semantics for the final symlink itself.
        let operation =
            resolve_future_path(absolute.parent().ok_or(AuthError::Protection)?)?.join(name);
        let mut names = vec![operation.clone(), resolve_future_path(&operation)?];
        if valid_registered_path(&absolute) {
            names.push(absolute);
        }
        if !names.iter().all(|path| valid_registered_path(path)) {
            return Err(AuthError::Protection);
        }
        let metadata = match std::fs::metadata(&operation) {
            Ok(metadata) => {
                require_regular(&metadata)?;
                Some(metadata)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(AuthError::Protection),
        };
        Ok(Self {
            operation,
            names,
            metadata,
        })
    }
}

/// Filesystem identity, including identities of replaced or deleted stores.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProtectedIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Immutable metadata supplied while the process-wide auth lock is held.
#[derive(Clone, Default)]
pub struct ProtectedPathsSnapshot {
    paths: BTreeSet<PathBuf>,
    identities: BTreeSet<ProtectedIdentity>,
}

impl ProtectedPathsSnapshot {
    pub fn paths(&self) -> &BTreeSet<PathBuf> {
        &self.paths
    }

    pub fn identities(&self) -> &BTreeSet<ProtectedIdentity> {
        &self.identities
    }
}

#[cfg(unix)]
#[derive(Default)]
pub(crate) struct Registry {
    snapshot: ProtectedPathsSnapshot,
    failed: bool,
    trusted_defaults: Option<Vec<PathBuf>>,
}

#[cfg(unix)]
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

#[cfg(unix)]
pub(crate) fn lock() -> Result<MutexGuard<'static, Registry>, AuthError> {
    let guard = REGISTRY
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_| AuthError::Protection)?;
    if guard.failed {
        return Err(AuthError::Protection);
    }
    Ok(guard)
}

/// Install the desktop owner's OS-resolved home before any protected reads.
/// This is process-lifetime authority, never a model-selected path. Rebinding
/// is refused; existing protected names and inode identities are retained.
#[cfg(unix)]
pub fn use_trusted_home(home: &Path) -> Result<(), AuthError> {
    if !home.is_absolute() || !valid_registered_path(home) {
        return Err(AuthError::Protection);
    }
    let defaults = vec![
        home.join(".polaris/auth.json"),
        home.join(".polaris/api_key.json"),
        home.join(".polaris/mcp/servers.json"),
        home.join(".polaris/mcp/secrets.json"),
    ];
    let mut registry = lock()?;
    if let Some(existing) = &registry.trusted_defaults {
        return if existing == &defaults {
            Ok(())
        } else {
            Err(AuthError::Protection)
        };
    }
    for path in &defaults {
        registry.register_store(path)?;
    }
    registry.register_subtree(home.join(".polaris/accounts"))?;
    registry.trusted_defaults = Some(defaults);
    Ok(())
}

/// Register one host-read source file (not a store) and its current inode.
/// Call before any model-visible reader can reach it.
#[cfg(unix)]
pub fn protect_path(path: &Path) -> Result<(), AuthError> {
    lock()?.register_path(path)
}

#[cfg(unix)]
pub fn trusted_home() -> Result<PathBuf, AuthError> {
    let registry = lock()?;
    registry
        .trusted_defaults
        .as_ref()
        .and_then(|paths| paths.first())
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
        .map(Path::to_path_buf)
        .ok_or(AuthError::Protection)
}

/// Runs a synchronous callback under the same lock as save/load/delete.
/// Default stores and their temporary paths are registered from `default_path`
/// on every call, unless the native owner installed its trusted home. Only
/// filesystem metadata is inspected; no body is read.
///
/// The callback must not reenter this API or any auth storage operation, or
/// wait for work that needs the auth lock. Complete snapshot copying inside
/// the callback: using a cloned value afterwards does not retain the lock.
#[cfg(unix)]
pub fn with_protected_paths<R>(
    callback: impl FnOnce(&ProtectedPathsSnapshot) -> R,
) -> Result<R, AuthError> {
    let account_home = trusted_home().ok();
    if let Some(home) = account_home {
        return crate::accounts::directory::with_reader_protection(&home, callback);
    }
    let mut registry = lock()?;
    with_protected_paths_locked(&mut registry, callback)
}

#[cfg(unix)]
fn with_protected_paths_locked<R>(
    registry: &mut Registry,
    callback: impl FnOnce(&ProtectedPathsSnapshot) -> R,
) -> Result<R, AuthError> {
    let defaults = match &registry.trusted_defaults {
        Some(paths) => paths.clone(),
        None => {
            let auth = crate::store::default_path()?;
            let parent = auth.parent().ok_or(AuthError::Protection)?;
            vec![
                parent.join("mcp/servers.json"),
                parent.join("mcp/secrets.json"),
                auth,
                crate::api_key::default_path()?,
            ]
        }
    };
    for path in &defaults {
        registry.register_store(path)?;
    }
    // Refresh registered names as well, retaining old identities after rotation.
    let paths: Vec<_> = registry.snapshot.paths.iter().cloned().collect();
    for path in paths {
        if path.file_name().is_some_and(|name| name == "accounts") {
            continue;
        }
        registry.register_path(&path)?;
    }
    Ok(callback(&registry.snapshot))
}

#[cfg(unix)]
pub(crate) fn merge_account_identities(
    identities: &BTreeSet<ProtectedIdentity>,
) -> Result<(), AuthError> {
    let mut registry = lock()?;
    for identity in identities {
        registry.remember_identity(*identity)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn with_account_identities<R>(
    identities: &BTreeSet<ProtectedIdentity>,
    callback: impl FnOnce(&ProtectedPathsSnapshot) -> R,
) -> Result<R, AuthError> {
    let mut registry = lock()?;
    for identity in identities {
        registry.remember_identity(*identity)?;
    }
    with_protected_paths_locked(&mut registry, callback)
}

/// Hold the protection lock while a host-owned store inspects or replaces files.
/// Register every destination before opening it and every new descriptor before
/// writing protected bytes. Never reenter protection or wait for asynchronous
/// work while this guard is held.
#[cfg(unix)]
pub struct StoreProtectionGuard(MutexGuard<'static, Registry>);

// Keep the public type usable in signatures, but its constructor private.
#[cfg(not(unix))]
pub struct StoreProtectionGuard {
    _private: (),
}

#[cfg(unix)]
pub fn protect_store(path: &Path) -> Result<StoreProtectionGuard, AuthError> {
    let mut guard = lock()?;
    guard.register_store(path)?;
    Ok(StoreProtectionGuard(guard))
}

impl StoreProtectionGuard {
    #[cfg(unix)]
    pub fn remember_file(&mut self, file: &File) -> Result<(), AuthError> {
        let metadata = file.metadata()?;
        require_regular(&metadata)?;
        self.0.remember(&metadata)
    }

    #[cfg(not(unix))]
    pub fn remember_file(&mut self, _file: &File) -> Result<(), AuthError> {
        Err(AuthError::Protection)
    }
}

// A snapshot or guard must not authorize I/O until native path/identity
// protection exists. Refuse before registry initialization or any callback.
#[cfg(not(unix))]
pub fn use_trusted_home(_home: &Path) -> Result<(), AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
pub fn protect_path(_path: &Path) -> Result<(), AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
pub fn trusted_home() -> Result<PathBuf, AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
pub fn with_protected_paths<R>(
    _callback: impl FnOnce(&ProtectedPathsSnapshot) -> R,
) -> Result<R, AuthError> {
    Err(AuthError::Protection)
}

#[cfg(not(unix))]
pub fn protect_store(_path: &Path) -> Result<StoreProtectionGuard, AuthError> {
    Err(AuthError::Protection)
}

#[cfg(unix)]
impl Registry {
    fn register_subtree(&mut self, path: PathBuf) -> Result<(), AuthError> {
        if !valid_registered_path(&path) {
            return Err(AuthError::Protection);
        }
        self.insert_path(path)
    }

    pub(crate) fn register_store(&mut self, path: &Path) -> Result<PathBuf, AuthError> {
        // Validate both names before mutating the persistent registry.
        let store = PreparedPath::new(path)?;
        let tmp = PreparedPath::new(&path.with_extension("json.tmp"))?;
        let operation = store.operation.clone();
        self.register_prepared(store)?;
        self.register_prepared(tmp)?;
        Ok(operation)
    }

    fn insert_path(&mut self, path: PathBuf) -> Result<(), AuthError> {
        if self.failed
            || path.as_os_str().len() > MAX_PATH_BYTES
            || (!self.snapshot.paths.contains(&path) && self.snapshot.paths.len() >= MAX_PATHS)
        {
            self.failed = true;
            return Err(AuthError::Protection);
        }
        self.snapshot.paths.insert(path);
        Ok(())
    }

    fn register_path(&mut self, path: &Path) -> Result<(), AuthError> {
        self.register_prepared(PreparedPath::new(path)?)
    }

    fn register_prepared(&mut self, path: PreparedPath) -> Result<(), AuthError> {
        for name in path.names {
            self.insert_path(name)?;
        }
        if let Some(metadata) = path.metadata {
            self.remember(&metadata)?;
        }
        Ok(())
    }

    pub(crate) fn remember(&mut self, metadata: &Metadata) -> Result<(), AuthError> {
        self.remember_identity(ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }

    fn remember_identity(&mut self, identity: ProtectedIdentity) -> Result<(), AuthError> {
        if self.failed
            || (!self.snapshot.identities.contains(&identity)
                && self.snapshot.identities.len() >= MAX_IDENTITIES)
        {
            self.failed = true;
            return Err(AuthError::Protection);
        }
        self.snapshot.identities.insert(identity);
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_auth_unavailable_protection_never_invokes_readers() {
        assert!(matches!(trusted_home(), Err(AuthError::Protection)));
        assert!(matches!(
            with_protected_paths(|_| panic!("unprotected reader was called")),
            Err(AuthError::Protection)
        ));
    }

    #[test]
    fn windows_auth_unavailable_protection_preserves_files_and_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("store.json");
        let temporary = existing.with_extension("json.tmp");
        std::fs::write(&existing, b"existing fixture").unwrap();
        std::fs::write(&temporary, b"interrupted fixture").unwrap();
        let missing = dir.path().join("missing").join("store.json");

        for path in [&existing, &missing] {
            assert!(matches!(protect_path(path), Err(AuthError::Protection)));
            assert!(matches!(protect_store(path), Err(AuthError::Protection)));
            assert!(matches!(
                use_trusted_home(path.parent().unwrap()),
                Err(AuthError::Protection)
            ));
        }
        assert_eq!(std::fs::read(&existing).unwrap(), b"existing fixture");
        assert_eq!(std::fs::read(&temporary).unwrap(), b"interrupted fixture");
        assert!(!missing.parent().unwrap().exists());
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Credentials, api_key, store};
    use std::os::unix::fs::FileTypeExt;

    fn credentials() -> Credentials {
        Credentials {
            access_token: "dummy-access".into(),
            refresh_token: "dummy-refresh".into(),
            account_id: "dummy-account".into(),
            expires_at: None,
        }
    }

    fn identity(path: &Path) -> ProtectedIdentity {
        let metadata = std::fs::metadata(path).unwrap();
        ProtectedIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    #[test]
    fn overrides_reserve_missing_store_and_tmp_before_first_save() {
        let dir = tempfile::tempdir().unwrap();
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            if api {
                assert!(api_key::load_from(&path).unwrap().is_none());
            } else {
                assert!(store::load_from(&path).unwrap().is_none());
            }
            with_protected_paths(|snapshot| {
                assert!(snapshot.paths().contains(&path));
                assert!(snapshot.paths().contains(&path.with_extension("json.tmp")));
                assert!(snapshot.paths().iter().all(|p| p.is_absolute()));
            })
            .unwrap();
            assert!(!path.exists());
        }
    }

    #[test]
    fn defaults_are_reserved_without_loading_a_body() {
        // The test process HOME is isolated by the test runner. No environment
        // mutation is needed inside this parallel test suite.
        with_protected_paths(|snapshot| {
            let parent = store::default_path()
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            for path in [
                store::default_path().unwrap(),
                api_key::default_path().unwrap(),
                parent.join("mcp/servers.json"),
                parent.join("mcp/secrets.json"),
            ] {
                assert!(
                    snapshot
                        .paths()
                        .contains(&std::path::absolute(&path).unwrap())
                );
                assert!(
                    snapshot
                        .paths()
                        .contains(&std::path::absolute(path.with_extension("json.tmp")).unwrap())
                );
            }
        })
        .unwrap();
    }

    #[test]
    fn preexisting_old_and_temporary_identities_survive_rotation_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            let tmp = path.with_extension("json.tmp");
            // These inodes existed before any loader registered them.
            std::fs::write(&path, b"dummy-old").unwrap();
            std::fs::hard_link(
                &path,
                dir.path().join(if api { "key-link" } else { "oauth-link" }),
            )
            .unwrap();
            std::fs::write(&tmp, b"dummy-interrupted").unwrap();
            let old = identity(&path);
            let temporary = identity(&tmp);
            if api {
                api_key::save_to(&path, "dummy-key").unwrap();
            } else {
                store::save_to(&path, &credentials()).unwrap();
            }
            let first = identity(&path);
            assert_eq!(first, temporary);
            if api {
                assert_eq!(
                    api_key::load_from(&path).unwrap().as_deref(),
                    Some("dummy-key")
                );
                api_key::save_to(&path, "dummy-key-2").unwrap();
            } else {
                assert_eq!(store::load_from(&path).unwrap(), Some(credentials()));
                store::save_to(&path, &credentials()).unwrap();
            }
            let second = identity(&path);
            assert_ne!(first, second);
            assert!(store::delete_at(&path).unwrap());
            with_protected_paths(|snapshot| {
                for id in [old, temporary, first, second] {
                    assert!(snapshot.identities().contains(&id));
                }
                assert!(snapshot.paths().contains(&path));
            })
            .unwrap();
        }
    }

    #[test]
    fn load_and_delete_register_preexisting_inodes() {
        let dir = tempfile::tempdir().unwrap();
        let oauth = dir.path().join("oauth");
        let key = dir.path().join("key");
        let deleted = dir.path().join("deleted");
        std::fs::write(&oauth, serde_json::to_vec(&credentials()).unwrap()).unwrap();
        std::fs::write(&key, br#"{"key":"dummy-key"}"#).unwrap();
        std::fs::write(&deleted, b"dummy-delete").unwrap();
        let ids = [identity(&oauth), identity(&key), identity(&deleted)];
        assert_eq!(store::load_from(&oauth).unwrap(), Some(credentials()));
        assert_eq!(
            api_key::load_from(&key).unwrap().as_deref(),
            Some("dummy-key")
        );
        assert!(store::delete_at(&deleted).unwrap());
        with_protected_paths(|snapshot| {
            assert!(ids.iter().all(|id| snapshot.identities().contains(id)));
        })
        .unwrap();
    }

    #[test]
    fn reserves_canonical_missing_paths_below_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let path = alias.join("future").join("auth");
        assert!(store::load_from(&path).unwrap().is_none());
        let resolved = real.canonicalize().unwrap().join("future").join("auth");
        with_protected_paths(|snapshot| {
            assert!(snapshot.paths().contains(&path));
            assert!(snapshot.paths().contains(&resolved));
            assert!(
                snapshot
                    .paths()
                    .contains(&resolved.with_extension("json.tmp"))
            );
        })
        .unwrap();
    }

    #[test]
    fn decode_errors_never_retain_input_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid");
        for body in [
            br#"{"key":true,"access_token":"DUMMY_SECRET","refresh_token":"r","account_id":"a","expires_at":"DUMMY_SECRET"}"#.as_slice(),
            br#""DUMMY_SECRET""#.as_slice(),
        ] {
            std::fs::write(&path, body).unwrap();
            for error in [store::load_from(&path).unwrap_err(), api_key::load_from(&path).unwrap_err()] {
                assert!(!format!("{error:?} {error}").contains("DUMMY_SECRET"));
            }
        }
    }

    #[test]
    fn bounded_registry_never_evicts_and_stays_failed_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::default();
        for i in 0..MAX_PATHS {
            registry
                .insert_path(dir.path().join(i.to_string()))
                .unwrap();
        }
        let existing = dir.path().join("0");
        registry.insert_path(existing.clone()).unwrap();
        assert!(registry.insert_path(dir.path().join("overflow")).is_err());
        assert!(registry.insert_path(existing).is_err());
        assert_eq!(registry.snapshot.paths.len(), MAX_PATHS);

        let mut registry = Registry::default();
        let file = dir.path().join("metadata");
        std::fs::write(&file, b"dummy").unwrap();
        let metadata = std::fs::metadata(&file).unwrap();
        for ino in 0..MAX_IDENTITIES as u64 {
            registry.snapshot.identities.insert(ProtectedIdentity {
                dev: metadata.dev().wrapping_add(1),
                ino,
            });
        }
        assert!(registry.remember(&metadata).is_err());
        assert!(registry.register_store(&file).is_err());
        assert_eq!(registry.snapshot.identities.len(), MAX_IDENTITIES);
        assert_eq!(std::fs::read(&file).unwrap(), b"dummy");
    }

    #[test]
    fn callback_holds_lock_against_both_loaders_and_delete() {
        use std::sync::mpsc;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth");
        store::save_to(&path, &credentials()).unwrap();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            with_protected_paths(|_| {
                assert!(matches!(
                    REGISTRY.get().unwrap().try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                for operation in 0..5 {
                    let started = started_tx.clone();
                    let done = done_tx.clone();
                    let path = path.clone();
                    let key = dir.path().join("key");
                    scope.spawn(move || {
                        started.send(()).unwrap();
                        match operation {
                            0 => {
                                store::save_to(&path, &credentials()).unwrap();
                            }
                            1 => {
                                store::load_from(&path).unwrap();
                            }
                            2 => {
                                store::delete_at(&path).unwrap();
                            }
                            3 => {
                                api_key::save_to(&key, "dummy-key").unwrap();
                            }
                            _ => {
                                api_key::load_from(&key).unwrap();
                            }
                        }
                        done.send(()).unwrap();
                    });
                }
                for _ in 0..5 {
                    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
            })
            .unwrap();
            for _ in 0..5 {
                done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        });
    }

    #[test]
    fn parent_components_resolve_existing_ancestors_without_poisoning_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        for api in [false, true] {
            let name = if api { "key" } else { "oauth" };
            let path = dir.path().join("config").join("..").join(name);
            let actual = dir.path().join(name);
            if api {
                api_key::save_to(&path, "dummy-key").unwrap();
                assert_eq!(
                    api_key::load_from(&path).unwrap().as_deref(),
                    Some("dummy-key")
                );
                assert_eq!(
                    api_key::load_from(&actual).unwrap().as_deref(),
                    Some("dummy-key")
                );
            } else {
                store::save_to(&path, &credentials()).unwrap();
                assert_eq!(store::load_from(&path).unwrap(), Some(credentials()));
                assert_eq!(store::load_from(&actual).unwrap(), Some(credentials()));
            }
            with_protected_paths(|snapshot| {
                assert!(snapshot.paths().iter().all(|p| valid_registered_path(p)));
                assert!(snapshot.paths().contains(&actual.canonicalize().unwrap()));
            })
            .unwrap();
            assert!(store::delete_at(&path).unwrap());
            assert!(!actual.exists());
        }
        // A subsequent ordinary snapshot must still succeed after deletion.
        with_protected_paths(|snapshot| {
            assert!(snapshot.paths().iter().all(|p| valid_registered_path(p)))
        })
        .unwrap();
    }

    #[test]
    fn parent_after_symlink_keeps_filesystem_meaning_and_final_link_rename() {
        let dir = tempfile::tempdir().unwrap();
        let actual = dir.path().join("actual");
        std::fs::create_dir_all(actual.join("config")).unwrap();
        std::os::unix::fs::symlink(actual.join("config"), dir.path().join("alias")).unwrap();
        let path = dir.path().join("alias/../key");
        api_key::save_to(&path, "dummy-real").unwrap();
        assert!(!dir.path().join("key").exists());
        assert_eq!(
            api_key::load_from(&actual.join("key")).unwrap().as_deref(),
            Some("dummy-real")
        );
        assert_eq!(
            api_key::load_from(&path).unwrap().as_deref(),
            Some("dummy-real")
        );

        let link = dir.path().join("last-link");
        std::os::unix::fs::symlink(actual.join("key"), &link).unwrap();
        api_key::save_to(&link, "dummy-new").unwrap();
        assert!(
            !std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            api_key::load_from(&actual.join("key")).unwrap().as_deref(),
            Some("dummy-real")
        );
        assert_eq!(
            api_key::load_from(&link).unwrap().as_deref(),
            Some("dummy-new")
        );
    }

    #[test]
    fn invalid_override_is_rejected_before_registration_or_io() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let invalid = [
            dir.path().join("missing/../key"),
            dir.path().join(std::ffi::OsStr::from_bytes(b"bad\0key")),
        ];
        let mut registry = Registry::default();
        for path in invalid {
            assert!(registry.register_store(&path).is_err());
            assert!(registry.snapshot.paths().is_empty());
            assert!(registry.snapshot.identities().is_empty());
            assert!(!registry.failed);
            assert!(store::load_from(&path).is_err());
            assert!(api_key::load_from(&path).is_err());
            assert!(store::save_to(&path, &credentials()).is_err());
            assert!(api_key::save_to(&path, "dummy").is_err());
            assert!(store::delete_at(&path).is_err());
        }
        assert!(!dir.path().join("missing").exists());
        assert!(!dir.path().join("key").exists());
        with_protected_paths(|snapshot| {
            assert!(snapshot.paths().iter().all(|p| valid_registered_path(p)))
        })
        .unwrap();
        let good = dir.path().join("good");
        api_key::save_to(&good, "dummy-good").unwrap();
        assert_eq!(
            api_key::load_from(&good).unwrap().as_deref(),
            Some("dummy-good")
        );
    }

    fn fifo(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a live NUL-terminated pathname in the test's TempDir.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn fifo_loads_and_tmp_saves_fail_without_blocking_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let pipe = dir.path().join("pipe");
        fifo(&pipe);
        for error in [
            store::load_from(&pipe).unwrap_err(),
            api_key::load_from(&pipe).unwrap_err(),
        ] {
            assert!(matches!(error, AuthError::Protection));
        }
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            let tmp = path.with_extension("json.tmp");
            fifo(&tmp);
            let error = if api {
                api_key::save_to(&path, "dummy")
            } else {
                store::save_to(&path, &credentials())
            }
            .unwrap_err();
            assert!(matches!(error, AuthError::Protection));
            assert!(!path.exists());
            assert!(std::fs::metadata(&tmp).unwrap().file_type().is_fifo());
        }
        with_protected_paths(|_| ()).unwrap();
        api_key::save_to(&dir.path().join("good"), "dummy").unwrap();
    }

    #[test]
    fn descriptor_check_rejects_a_fifo_substituted_after_precheck() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race");
        std::fs::write(&path, b"dummy").unwrap();
        require_regular(&std::fs::metadata(&path).unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        fifo(&path);
        // Enter the post-precheck stage directly: neither open can wait for a
        // peer, and a successful open must still reject the descriptor type.
        assert!(matches!(
            open_regular_checked(&path, false),
            Err(AuthError::Protection)
        ));
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        assert!(matches!(
            open_regular_checked(&path, true),
            Err(AuthError::Protection)
        ));
    }

    #[test]
    fn body_limit_accepts_boundary_and_rejects_oversize_without_poisoning_lock() {
        let dir = tempfile::tempdir().unwrap();
        for api in [false, true] {
            let path = dir.path().join(if api { "key" } else { "oauth" });
            let mut body = if api {
                br#"{"key":"dummy"}"#.to_vec()
            } else {
                serde_json::to_vec(&credentials()).unwrap()
            };
            body.resize(MAX_BODY_BYTES as usize, b' ');
            std::fs::write(&path, &body).unwrap();
            if api {
                assert_eq!(api_key::load_from(&path).unwrap().as_deref(), Some("dummy"));
            } else {
                assert_eq!(store::load_from(&path).unwrap(), Some(credentials()));
            }
            // One byte over the boundary, then a sparse huge input. Stat must
            // reject the latter before allocating its body.
            for length in [MAX_BODY_BYTES + 1, 1 << 32] {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(length)
                    .unwrap();
                let error = if api {
                    api_key::load_from(&path).map(|_| ())
                } else {
                    store::load_from(&path).map(|_| ())
                }
                .unwrap_err();
                assert!(matches!(error, AuthError::Protection));
                assert_eq!(
                    error.to_string(),
                    "authentication path protection is unavailable"
                );
                with_protected_paths(|_| ()).unwrap();
            }
        }
    }
}
