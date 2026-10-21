//! Explicit offline import and lossless repair of fixed decision generations.
use super::*;
use std::{
    ffi::CString,
    fs::OpenOptions,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

pub(super) fn import(
    subject: &Subject,
    home: &Path,
    worker: &Path,
    model: &Path,
    replacement: Option<&ReplacementPlan>,
    mut cancelled: impl FnMut() -> bool,
    before_publish: impl FnOnce() -> Result<(), ManagedDecisionError>,
) -> Result<(), ManagedDecisionError> {
    use ManagedDecisionError::{Cancelled, Conflict, Install, Integrity};
    if cancelled() {
        return Err(Cancelled);
    }
    if matches!(subject.device, Device::Metal) {
        return Err(ManagedDecisionError::UnsupportedDevice);
    }
    if !home.is_absolute() || home.canonicalize().ok().as_deref() != Some(home) {
        return Err(Integrity);
    }
    let directory =
        crate::project_mutation::open_directory_nofollow(home).map_err(|_| Integrity)?;
    let metadata = directory.metadata().map_err(|_| Install)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7022 != 0 {
        return Err(Integrity);
    }
    let mut chain = vec![(home.to_owned(), directory)];
    for name in [".polaris", "decision-runtime"] {
        create_child(&mut chain, name)?;
    }
    // Serialize cooperating management processes; inference never takes this
    // lock and a busy management operation is not automatically retried.
    let runtime = chain
        .last()
        .ok_or(Integrity)?
        .1
        .try_clone()
        .map_err(|_| Install)?;
    let management = management_lock(&runtime)?;
    create_child(&mut chain, "runs")?;
    chain.pop();
    create_child(&mut chain, "generations")?;
    verify_chain(&chain)?;
    let (parent_path, parent) = chain.last().ok_or(Integrity)?;
    let required = subject
        .worker
        .bytes
        .checked_add(subject.model.bytes)
        .and_then(|n| n.checked_add(256 * 1024 * 1024))
        .ok_or(Integrity)?;
    let mut space = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: fstatvfs writes the allocated struct; read only on success.
    if unsafe { libc::fstatvfs(parent.as_raw_fd(), space.as_mut_ptr()) } != 0 {
        return Err(Install);
    }
    let space = unsafe { space.assume_init() };
    let available = u64::from(space.f_bavail)
        .checked_mul(space.f_frsize)
        .ok_or(Integrity)?;
    if available < required {
        return Err(ManagedDecisionError::InsufficientSpace);
    }
    let target = CString::new(subject.generation.as_str()).map_err(|_| Integrity)?;
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    let exists = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            target.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0;
    if exists && replacement.is_none() {
        return Err(Conflict);
    }
    if !exists && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
        return Err(Install);
    }
    let previous = if let Some(expected) = replacement {
        Some(open_replacement(
            parent_path,
            parent,
            &subject.generation,
            expected,
        )?)
    } else {
        None
    };
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| Install)?;
    let random_name = format!(
        ".pending-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let name = match replacement {
        Some(expected) => expected
            .backup_destination
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(Integrity)?
            .to_owned(),
        None => random_name,
    };
    let name_c = CString::new(name.as_str()).map_err(|_| Integrity)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
        return Err(Conflict);
    }
    let staging = polaris_tools::path_policy::tree_admission::open_at(
        parent,
        name.as_ref(),
        true,
        &parent_path.join(&name),
    )
    .map_err(|_| Integrity)?;
    private(&staging)?;
    let mut published = false;
    let result = (|| {
        copy(
            &staging,
            "worker",
            worker,
            &subject.worker,
            0o500,
            &mut cancelled,
        )?;
        copy(
            &staging,
            "model.gguf",
            model,
            &subject.model,
            0o400,
            &mut cancelled,
        )?;
        staging.sync_all().map_err(|_| Install)?;
        verify_chain(&chain)?;
        before_publish()?;
        if cancelled() {
            return Err(Cancelled);
        }
        verify_chain(&chain)?;
        same_directory(parent, &name, &staging)?;
        verify_management_lock(&runtime, &management)?;
        if let Some(expected) = replacement {
            // Consent is bound to this existing directory, not just its name.
            let _ = open_replacement(parent_path, parent, &subject.generation, expected)?;
        }
        // Exclusive atomic directory publication: a concurrent importer or a
        // previous generation is never overwritten. No active config changes.
        if unsafe {
            libc::renameatx_np(
                parent.as_raw_fd(),
                name_c.as_ptr(),
                parent.as_raw_fd(),
                target.as_ptr(),
                if replacement.is_some() {
                    libc::RENAME_SWAP
                } else {
                    libc::RENAME_EXCL
                },
            )
        } != 0
        {
            return Err(Conflict);
        }
        published = true;
        parent.sync_all().map_err(|_| Install)?;
        verify_chain(&chain)?;
        same_directory(parent, &subject.generation, &staging)?;
        if let Some(previous) = &previous {
            same_directory(parent, &name, previous)?;
        }
        Ok(())
    })();
    // Clean only our unpublished staging directory through its pinned FD.
    // After publication (including a durability/reporting error), preserve the
    // complete generation for inspection/reuse; never undo another install.
    if !published && same_directory(parent, &name, &staging).is_ok() {
        for leaf in [c"worker", c"model.gguf"] {
            unsafe {
                libc::unlinkat(staging.as_raw_fd(), leaf.as_ptr(), 0);
            }
        }
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR);
        }
        let _ = parent.sync_all();
    }
    result
}

fn management_lock(runtime: &File) -> Result<File, ManagedDecisionError> {
    use ManagedDecisionError::{Conflict, Integrity};
    let fd = unsafe {
        libc::openat(
            runtime.as_raw_fd(),
            c"management.lock".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(Integrity);
    }
    // SAFETY: transfer the fresh owned descriptor once.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(|_| Integrity)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7177 != 0
        || metadata.nlink() != 1
        || metadata.len() != 0
    {
        return Err(Integrity);
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(Conflict);
    }
    verify_management_lock(runtime, &file)?;
    Ok(file)
}

fn verify_management_lock(runtime: &File, expected: &File) -> Result<(), ManagedDecisionError> {
    let current = polaris_tools::path_policy::tree_admission::open_at(
        runtime,
        "management.lock".as_ref(),
        false,
        Path::new("management.lock"),
    )
    .map_err(|_| ManagedDecisionError::Integrity)?;
    let a = current
        .metadata()
        .map_err(|_| ManagedDecisionError::Integrity)?;
    let b = expected
        .metadata()
        .map_err(|_| ManagedDecisionError::Integrity)?;
    if !same_file_metadata(&a, &b) {
        return Err(ManagedDecisionError::Integrity);
    }
    Ok(())
}

fn identity(metadata: &std::fs::Metadata) -> GenerationIdentity {
    GenerationIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}

pub(super) fn replacement(
    home: &Path,
    generation: &str,
    nonce: &str,
) -> Result<ReplacementPlan, ManagedDecisionError> {
    use ManagedDecisionError::Integrity;
    if !valid_profile_id(generation)
        || nonce.len() != 32
        || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
        || !home.is_absolute()
        || home.canonicalize().ok().as_deref() != Some(home)
    {
        return Err(Integrity);
    }
    let directory =
        crate::project_mutation::open_directory_nofollow(home).map_err(|_| Integrity)?;
    let m = directory.metadata().map_err(|_| Integrity)?;
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o7022 != 0 {
        return Err(Integrity);
    }
    let mut chain = vec![(home.to_owned(), directory)];
    for name in [".polaris", "decision-runtime", "generations", generation] {
        descend(&mut chain, name)?;
    }
    verify_chain(&chain)?;
    let m = chain
        .last()
        .ok_or(Integrity)?
        .1
        .metadata()
        .map_err(|_| Integrity)?;
    Ok(ReplacementPlan {
        backup_destination: home
            .join(".polaris/decision-runtime/generations")
            .join(format!(".recovery-{nonce}")),
        identity: identity(&m),
    })
}

fn open_replacement(
    parent_path: &Path,
    parent: &File,
    generation: &str,
    expected: &ReplacementPlan,
) -> Result<File, ManagedDecisionError> {
    use ManagedDecisionError::{Integrity, PlanChanged};
    if expected.backup_destination.parent() != Some(parent_path) {
        return Err(Integrity);
    }
    let file = polaris_tools::path_policy::tree_admission::open_at(
        parent,
        generation.as_ref(),
        true,
        &parent_path.join(generation),
    )
    .map_err(|_| PlanChanged)?;
    private(&file)?;
    let m = file.metadata().map_err(|_| Integrity)?;
    if identity(&m) != expected.identity {
        return Err(PlanChanged);
    }
    Ok(file)
}

pub(super) fn create_child(
    chain: &mut Vec<(PathBuf, File)>,
    name: &str,
) -> Result<(), ManagedDecisionError> {
    let parent = &chain.last().ok_or(ManagedDecisionError::Integrity)?.1;
    let component = CString::new(name).map_err(|_| ManagedDecisionError::Integrity)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) } != 0
        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(ManagedDecisionError::Install);
    }
    parent
        .sync_all()
        .map_err(|_| ManagedDecisionError::Install)?;
    descend(chain, name)
}

pub(super) fn verify_chain(chain: &[(PathBuf, File)]) -> Result<(), ManagedDecisionError> {
    for (index, (path, file)) in chain.iter().enumerate() {
        let observed = path
            .symlink_metadata()
            .map_err(|_| ManagedDecisionError::Integrity)?;
        let pinned = file
            .metadata()
            .map_err(|_| ManagedDecisionError::Integrity)?;
        let forbidden_mode = if index == 0 { 0o7022 } else { 0o7077 };
        if !observed.is_dir()
            || !pinned.is_dir()
            || pinned.uid() != unsafe { libc::geteuid() }
            || pinned.mode() & forbidden_mode != 0
            || (observed.dev(), observed.ino()) != (pinned.dev(), pinned.ino())
        {
            return Err(ManagedDecisionError::Integrity);
        }
    }
    Ok(())
}

pub(super) fn same_directory(
    parent: &File,
    name: &str,
    expected: &File,
) -> Result<(), ManagedDecisionError> {
    let opened = polaris_tools::path_policy::tree_admission::open_at(
        parent,
        name.as_ref(),
        true,
        Path::new(name),
    )
    .map_err(|_| ManagedDecisionError::Integrity)?;
    let a = opened
        .metadata()
        .map_err(|_| ManagedDecisionError::Integrity)?;
    let b = expected
        .metadata()
        .map_err(|_| ManagedDecisionError::Integrity)?;
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err(ManagedDecisionError::Integrity);
    }
    Ok(())
}

fn copy(
    directory: &File,
    name: &str,
    source: &Path,
    expected: &LockedFile,
    mode: libc::mode_t,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), ManagedDecisionError> {
    use ManagedDecisionError::{Cancelled, Install, Integrity};
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(source)
        .map_err(|_| Integrity)?;
    let before = source.metadata().map_err(|_| Integrity)?;
    if !before.is_file() || before.len() != expected.bytes {
        return Err(Integrity);
    }
    let name = CString::new(name).map_err(|_| Integrity)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(Install);
    }
    // SAFETY: a fresh owned descriptor from openat, transferred exactly once.
    let mut output = unsafe { File::from_raw_fd(fd) };
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        if cancelled() {
            return Err(Cancelled);
        }
        let n = source.read(&mut buffer).map_err(|_| Integrity)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).ok_or(Integrity)?;
        if count > expected.bytes {
            return Err(Integrity);
        }
        hash.update(&buffer[..n]);
        output.write_all(&buffer[..n]).map_err(|_| Install)?;
    }
    let after = source.metadata().map_err(|_| Integrity)?;
    if count != expected.bytes
        || format!("{:x}", hash.finalize()) != expected.sha256
        || (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec(),
        )
    {
        return Err(Integrity);
    }
    if unsafe { libc::fchmod(output.as_raw_fd(), mode) } != 0 {
        return Err(Install);
    }
    output.sync_all().map_err(|_| Install)
}

#[cfg(test)]
mod tests;
