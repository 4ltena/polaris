//! FD-fixed copy kernel with separate ephemeral and persistent lifetimes.
use super::*;
use polaris_mcp::entry_gate::ProductionEntryLeaseV2;
use polaris_tools::entry_kernel::{EntryAnchorV2, ProductionBinaryIdentityV2};
use std::os::fd::IntoRawFd;

pub(super) struct RuntimeHelper {
    pub(super) runtime: File,
    output: File,
    parent: File,
    pub(super) helper: PathBuf,
}

impl RuntimeHelper {
    pub(super) fn create(root: &Path, parent: &File, secrets: &RegisteredSecrets) -> Result<Self> {
        check_destination(root, parent, secrets)?;
        reserve_destination(&root.join("runtime/polaris"), secrets)?;
        private_run(parent)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), c"runtime".as_ptr(), 0o700) } != 0 {
            return Err(PrepareError::Io);
        }
        let runtime = open_at(parent, c"runtime", true, false)?;
        check_destination(&root.join("runtime"), &runtime, secrets)?;
        let output = open_at(&runtime, c"polaris", false, true)?;
        check_destination(&root.join("runtime/polaris"), &output, secrets)?;
        Ok(Self {
            runtime,
            output,
            parent: io(parent.try_clone())?,
            helper: root.join("runtime/polaris"),
        })
    }

    pub(super) fn populate_and_seal(
        &mut self,
        input: &mut File,
        executable: &Path,
        before: &Metadata,
        secrets: &RegisteredSecrets,
    ) -> Result<()> {
        self.check_destinations(secrets)?;
        let copied = io(std::io::copy(
            &mut (&mut *input).take(MAX_HELPER_BYTES + 1),
            &mut self.output,
        ))?;
        if copied > MAX_HELPER_BYTES || copied != before.len() {
            return Err(PrepareError::Limit);
        }
        io(self.output.flush())?;
        io(self.output.sync_all())?;
        staging_fault("file-sync")?;
        if !stable(before, &io(input.metadata())?)
            || !stable(before, &io(open_physical(executable, false)?.metadata())?)
        {
            return Err(PrepareError::Changed);
        }
        io(self.output.set_permissions(Permissions::from_mode(0o500)))?;
        staging_fault("leaf-mode")?;
        io(self.output.sync_all())?;
        io(self.runtime.set_permissions(Permissions::from_mode(0o500)))?;
        staging_fault("runtime-mode")?;
        io(self.runtime.sync_all())?;
        staging_fault("runtime-sync")?;
        io(self.parent.sync_all())?;
        let source = ProductionBinaryIdentityV2::inspect(executable)
            .map_err(|_| PrepareError::UnsafeHelper)?;
        self.verify_copy(&source.sha256, secrets)?;
        if !stable(before, &io(input.metadata())?)
            || !stable(before, &io(open_physical(executable, false)?.metadata())?)
        {
            return Err(PrepareError::Changed);
        }
        Ok(())
    }

    fn existing(root: &Path, parent: &File, secrets: &RegisteredSecrets) -> Result<Self> {
        check_destination(root, parent, secrets)?;
        let runtime = open_at(parent, c"runtime", true, false)?;
        check_destination(&root.join("runtime"), &runtime, secrets)?;
        let output = open_at(&runtime, c"polaris", false, false)?;
        check_destination(&root.join("runtime/polaris"), &output, secrets)?;
        Ok(Self {
            runtime,
            output,
            parent: io(parent.try_clone())?,
            helper: root.join("runtime/polaris"),
        })
    }

    fn check_destinations(&self, secrets: &RegisteredSecrets) -> Result<()> {
        let runtime_path = self.helper.parent().ok_or(PrepareError::InvalidPath)?;
        let run_path = runtime_path.parent().ok_or(PrepareError::InvalidPath)?;
        check_destination(run_path, &self.parent, secrets)?;
        check_destination(runtime_path, &self.runtime, secrets)?;
        check_destination(&self.helper, &self.output, secrets)
    }

    fn verify(&self, digest: &str, secrets: &RegisteredSecrets) -> Result<()> {
        self.check_destinations(secrets)?;
        exact_entries(&self.parent, &[b"runtime"])?;
        self.verify_copy(digest, secrets)
    }

    // Snapshot runs also contain workspace/home/tmp siblings. The persistent
    // owner separately requires its run to contain only this runtime directory.
    fn verify_copy(&self, digest: &str, secrets: &RegisteredSecrets) -> Result<()> {
        self.check_destinations(secrets)?;
        private_run(&self.parent)?;
        exact_entries(&self.runtime, &[b"polaris"])?;
        let binary =
            ProductionBinaryIdentityV2::inspect(&self.helper).map_err(|_| PrepareError::Changed)?;
        let anchors = polaris_tools::entry_kernel::runtime_copy_anchors(&binary)
            .map_err(|_| PrepareError::Changed)?;
        if binary.sha256 != digest
            || binary.anchor != EntryAnchorV2::of(&io(self.output.metadata())?)
            || anchors.0 != EntryAnchorV2::of(&io(self.runtime.metadata())?)
            || anchors.1 != EntryAnchorV2::of(&io(self.parent.metadata())?)
        {
            return Err(PrepareError::Changed);
        }
        Ok(())
    }
}

pub(super) struct EphemeralRuntimeHelper(pub(super) RuntimeHelper);
impl Drop for EphemeralRuntimeHelper {
    fn drop(&mut self) {
        // Only the Snapshot wrapper cleans up; persistent copies keep 0500.
        let _ = self
            .0
            .runtime
            .set_permissions(Permissions::from_mode(0o700));
    }
}

/// Retain across all helper children. Completed paths survive owner drop so a
/// delayed child remains identifiable by the native process inventory.
pub struct PreparedConfinedHelper {
    helper: PathBuf,
    _source: File,
    _source_lease: ProductionEntryLeaseV2,
    _copy_lease: Option<ProductionEntryLeaseV2>,
    _runtime: Option<RuntimeHelper>,
    _state: Option<File>,
}

impl PreparedConfinedHelper {
    pub fn prepare(policy: &SandboxPolicy, state: &Path, executable: &Path) -> Result<Self> {
        let home = crate::os_home::trusted_user_home().map_err(|_| PrepareError::EntryGate)?;
        polaris_auth::protection::with_protected_paths(|protected| {
            prepare_confined_locked(policy, state, executable, &home, protected)
        })
        .map_err(|_| PrepareError::ProtectionUnavailable)?
    }

    pub fn helper_path(&self) -> &Path {
        &self.helper
    }
}

fn prepare_confined_locked(
    policy: &SandboxPolicy,
    state: &Path,
    executable: &Path,
    home: &Path,
    protected: &ProtectedPathsSnapshot,
) -> Result<PreparedConfinedHelper> {
    physical(executable)?;
    let mut input = open_physical(executable, false)?;
    let before = io(input.metadata())?;
    helper_metadata(&before)?;
    let secrets = RegisteredSecrets::from_auth_registry(protected)
        .map_err(|_| PrepareError::ProtectionUnavailable)?;
    if secrets.denies(executable, &before) || polaris_tools::path_policy::is_denied(executable) {
        return Err(PrepareError::UnsafeHelper);
    }
    let source_lease =
        ProductionEntryLeaseV2::enter(home, executable).map_err(|_| PrepareError::EntryGate)?;
    let source =
        ProductionBinaryIdentityV2::inspect(executable).map_err(|_| PrepareError::UnsafeHelper)?;
    let mut prepared = PreparedConfinedHelper {
        helper: executable.to_owned(),
        _source: io(input.try_clone())?,
        _source_lease: source_lease,
        _copy_lease: None,
        _runtime: None,
        _state: None,
    };
    if !writable_overlap(
        policy,
        executable.parent().ok_or(PrepareError::InvalidPath)?,
    )? {
        source.verify().map_err(|_| PrepareError::Changed)?;
        prepared
            ._source_lease
            .verify()
            .map_err(|_| PrepareError::EntryGate)?;
        if !stable(&before, &io(input.metadata())?)
            || !stable(&before, &io(open_physical(executable, false)?.metadata())?)
        {
            return Err(PrepareError::Changed);
        }
        return Ok(prepared);
    }
    physical(state)?;
    if writable_overlap(policy, state)? {
        return Err(PrepareError::RuntimeDenied);
    }
    let state_fd = open_physical(state, true)?;
    check_destination(state, &state_fd, &secrets)?;
    let state_meta = io(state_fd.metadata())?;
    if state_meta.uid() != unsafe { libc::geteuid() } || state_meta.mode() & 0o022 != 0 {
        return Err(PrepareError::InvalidPath);
    }
    let final_name = CString::new(format!("polaris-helper-{}", source.sha256)).unwrap();
    let final_root = state.join(final_name.to_str().unwrap());
    let runtime = match open_at(&state_fd, &final_name, true, false) {
        Ok(root) => RuntimeHelper::existing(&final_root, &root, &secrets)?,
        Err(_) => {
            // Absence is tested without following aliases; every other error
            // remains a refusal rather than an invitation to overwrite.
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            let result = unsafe {
                libc::fstatat(
                    state_fd.as_raw_fd(),
                    final_name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
                return Err(PrepareError::Changed);
            }
            let mut temp = TempRun::create(state, &state_fd, &secrets)?;
            temp.runtime = Some(RuntimeHelper::create(&temp.path, &temp.root, &secrets)?);
            temp.runtime
                .as_mut()
                .unwrap()
                .populate_and_seal(&mut input, executable, &before, &secrets)?;
            source.verify().map_err(|_| PrepareError::Changed)?;
            prepared
                ._source_lease
                .verify()
                .map_err(|_| PrepareError::EntryGate)?;
            state_identity(state, &state_meta)?;
            staging_fault("before-publish")?;
            #[cfg(test)]
            if EXTRA_CHILD.with(|extra| extra.get()) {
                io(fs::write(
                    temp.path.join("unknown"),
                    b"must remain unpublished",
                ))?;
            }
            temp.runtime
                .as_ref()
                .unwrap()
                .verify(&source.sha256, &secrets)?;
            match publish_no_replace(&state_fd, &temp.name, &final_name) {
                Ok(()) => {
                    temp.published = true;
                    staging_fault("after-publish")?;
                    io(state_fd.sync_all())?;
                    staging_fault("state-sync")?;
                }
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                Err(_) => return Err(PrepareError::Io),
            }
            let root = open_at(&state_fd, &final_name, true, false)?;
            RuntimeHelper::existing(&final_root, &root, &secrets)?
        }
    };
    runtime.verify(&source.sha256, &secrets)?;
    // Reuse also closes a prior crash between rename and parent fsync.
    io(state_fd.sync_all())?;
    source.verify().map_err(|_| PrepareError::Changed)?;
    if !stable(&before, &io(input.metadata())?) {
        return Err(PrepareError::Changed);
    }
    state_identity(state, &state_meta)?;
    let copy_lease = ProductionEntryLeaseV2::enter(home, &runtime.helper)
        .map_err(|_| PrepareError::EntryGate)?;
    prepared
        ._source_lease
        .verify()
        .map_err(|_| PrepareError::EntryGate)?;
    runtime.verify(&source.sha256, &secrets)?;
    prepared.helper = runtime.helper.clone();
    prepared._copy_lease = Some(copy_lease);
    prepared._runtime = Some(runtime);
    prepared._state = Some(state_fd);
    Ok(prepared)
}

fn private_run(directory: &File) -> Result<()> {
    let metadata = io(directory.metadata())?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(PrepareError::InvalidPath);
    }
    Ok(())
}

fn state_identity(path: &Path, before: &Metadata) -> Result<()> {
    let after = io(open_physical(path, true)?.metadata())?;
    if !identity(before, &after) || before.uid() != after.uid() || before.mode() != after.mode() {
        return Err(PrepareError::Changed);
    }
    Ok(())
}

fn exact_entries(directory: &File, expected: &[&[u8]]) -> Result<()> {
    let before = io(directory.metadata())?;
    let duplicate = open_at(directory, c".", true, false)?;
    let fd = duplicate.into_raw_fd();
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe { libc::close(fd) };
        return Err(PrepareError::Io);
    }
    let mut found = Vec::new();
    let error = loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0
        };
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0
        };
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break std::io::Error::last_os_error().raw_os_error() != Some(0);
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if found.len() >= expected.len() {
            break true;
        }
        found.push(name.to_vec());
    };
    unsafe { libc::closedir(stream) };
    found.sort();
    let mut expected = expected
        .iter()
        .map(|name| name.to_vec())
        .collect::<Vec<_>>();
    expected.sort();
    if error || found != expected || !stable(&before, &io(directory.metadata())?) {
        return Err(PrepareError::Changed);
    }
    Ok(())
}

fn writable_overlap(policy: &SandboxPolicy, directory: &Path) -> Result<bool> {
    let metadata = io(open_physical(directory, true)?.metadata())?;
    for root in policy.writable_roots() {
        physical(root)?;
        let root_metadata = io(open_physical(root, true)?.metadata())?;
        if overlaps(directory, root)
            || ancestor_has_identity(directory, &root_metadata)?
            || ancestor_has_identity(root, &metadata)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reserve_destination(path: &Path, secrets: &RegisteredSecrets) -> Result<()> {
    if secrets.reserves(path) || polaris_tools::path_policy::is_denied(path) {
        return Err(PrepareError::UnsafeHelper);
    }
    Ok(())
}

fn check_destination(path: &Path, file: &File, secrets: &RegisteredSecrets) -> Result<()> {
    reserve_destination(path, secrets)?;
    if secrets.denies(path, &io(file.metadata())?) {
        return Err(PrepareError::UnsafeHelper);
    }
    Ok(())
}

struct TempRun {
    name: CString,
    path: PathBuf,
    root: File,
    state: File,
    runtime: Option<RuntimeHelper>,
    published: bool,
    secrets: RegisteredSecrets,
}
impl TempRun {
    fn create(state: &Path, parent: &File, secrets: &RegisteredSecrets) -> Result<Self> {
        check_destination(state, parent, secrets)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| PrepareError::Io)?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = CString::new(format!(".polaris-helper-{suffix}.tmp")).unwrap();
        reserve_destination(&state.join(name.to_str().unwrap()), secrets)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(PrepareError::Io);
        }
        let root = open_at(parent, &name, true, false)?;
        io(root.set_permissions(Permissions::from_mode(0o700)))?;
        Ok(Self {
            path: state.join(name.to_str().unwrap()),
            name,
            root,
            state: io(parent.try_clone())?,
            runtime: None,
            published: false,
            secrets: secrets.clone(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        if self.published {
            return Ok(());
        }
        check_destination(
            self.path.parent().ok_or(PrepareError::InvalidPath)?,
            &self.state,
            &self.secrets,
        )?;
        check_destination(&self.path, &self.root, &self.secrets)?;
        let named = open_at(&self.state, &self.name, true, false)?;
        if !stable(&io(self.root.metadata())?, &io(named.metadata())?) {
            return Err(PrepareError::Changed);
        }
        private_run(&self.root)?;
        if let Some(runtime) = &self.runtime {
            runtime.check_destinations(&self.secrets)?;
            exact_entries(&self.root, &[b"runtime"])?;
            exact_entries(&runtime.runtime, &[b"polaris"])?;
            let named_runtime = open_at(&self.root, c"runtime", true, false)?;
            let named_leaf = open_at(&runtime.runtime, c"polaris", false, false)?;
            let leaf = io(runtime.output.metadata())?;
            let directory = io(runtime.runtime.metadata())?;
            if !stable(&directory, &io(named_runtime.metadata())?)
                || !stable(&leaf, &io(named_leaf.metadata())?)
                || !leaf.is_file()
                || leaf.nlink() != 1
                || leaf.uid() != unsafe { libc::geteuid() }
                || ![0o600, 0o500].contains(&(leaf.mode() & 0o7777))
                || ![0o700, 0o500].contains(&(directory.mode() & 0o7777))
                || directory.uid() != unsafe { libc::geteuid() }
            {
                return Err(PrepareError::Changed);
            }
            io(runtime
                .runtime
                .set_permissions(Permissions::from_mode(0o700)))?;
            if unsafe { libc::unlinkat(runtime.runtime.as_raw_fd(), c"polaris".as_ptr(), 0) } != 0
                || unsafe {
                    libc::unlinkat(
                        self.root.as_raw_fd(),
                        c"runtime".as_ptr(),
                        libc::AT_REMOVEDIR,
                    )
                } != 0
            {
                return Err(PrepareError::Io);
            }
        } else {
            exact_entries(&self.root, &[])?;
        }
        if unsafe {
            libc::unlinkat(
                self.state.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(PrepareError::Io);
        }
        io(self.state.sync_all())
    }
}
impl Drop for TempRun {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn publish_no_replace(
    parent: &File,
    from: &std::ffi::CStr,
    to: &std::ffi::CStr,
) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn staging_fault(_point: &'static str) -> Result<()> {
    #[cfg(test)]
    if FAULT.with(|fault| fault.get() == Some(_point)) {
        if CRASH.with(|crash| crash.get()) {
            unsafe { libc::_exit(91) };
        }
        return Err(PrepareError::Io);
    }
    Ok(())
}
#[cfg(test)]
thread_local! { static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; static CRASH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; static EXTRA_CHILD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[cfg(test)]
#[path = "helper_staging_tests.rs"]
mod tests;
