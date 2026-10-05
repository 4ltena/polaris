//! M7b-5: what this device remembers about a shared project. One small file
//! per project, next to the rest of its state. It holds no credential: the
//! remote it names is reached with the credentials Git already has.
use super::share::{ShareLink, ShareManifest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub const FILE: &str = "participation.json";

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod generations_v1 {
    use super::*;
    use crate::{
        apply_intent::{DIRECTORY_OVERHEAD_BYTES, FREE_SPACE_FLOOR_BYTES, allocated_bytes},
        management_effect::ManagementEffectHashV1 as Hash,
    };
    use sha2::{Digest, Sha256};

    #[derive(Debug, thiserror::Error)]
    pub enum C12MutationErrorV1 {
        #[error("C12 capacity unavailable: {available_bytes} < {required_bytes}")]
        ResourceUnavailable {
            required_bytes: u64,
            available_bytes: u64,
        },
        #[error("C12 capacity arithmetic overflow")]
        Overflow,
        #[error("invalid C12 evidence: {0}")]
        CorruptEvidence(&'static str),
        #[error(transparent)]
        Io(#[from] io::Error),
    }
    type Result<T> = std::result::Result<T, C12MutationErrorV1>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ParticipationPublicationV1 {
        Baseline,
        Join,
        Write,
        Leave,
        Apply,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ParticipationGenerationV1 {
        pub schema_version: u8,
        pub generation: u64,
        pub mutation_id: Option<Hash>,
        pub participation_hash: Option<Hash>,
        pub previous_marker_hash: Option<Hash>,
        pub publication: ParticipationPublicationV1,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct C12PendingV1 {
        pub schema_version: u8,
        pub base_generation: u64,
        pub base_marker_hash: Hash,
        pub target_marker: ParticipationGenerationV1,
        pub target_marker_hash: Hash,
        pub target_body_hash: Option<Hash>,
        pub target_body_bytes: u64,
        pub publication: ParticipationPublicationV1,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct BaselineV1 {
        schema_version: u8,
        baseline_marker_hash: Hash,
    }

    fn domain_hash(domain: &[u8], bytes: &[u8]) -> Hash {
        let mut digest = Sha256::new();
        digest.update(domain);
        digest.update(bytes);
        let hex: String = digest
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Hash::from_hex(&hex).expect("SHA-256 fixed-width lowercase hash")
    }

    pub fn participation_bytes_hash(bytes: &[u8]) -> Hash {
        domain_hash(b"polaris-sync-participation-bytes-v1\0", bytes)
    }

    fn decode_exact<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T> {
        let value: T = serde_json::from_slice(bytes)
            .map_err(|_| C12MutationErrorV1::CorruptEvidence("schema"))?;
        if serde_json::to_vec(&value).map_err(|_| C12MutationErrorV1::CorruptEvidence("encode"))?
            != bytes
        {
            return Err(C12MutationErrorV1::CorruptEvidence("noncanonical"));
        }
        Ok(value)
    }

    impl ParticipationGenerationV1 {
        pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
            use ParticipationPublicationV1::*;
            if self.schema_version != 1
                || [
                    self.mutation_id,
                    self.participation_hash,
                    self.previous_marker_hash,
                ]
                .into_iter()
                .flatten()
                .any(|hash| hash.as_hex().bytes().all(|byte| byte == b'0'))
                || (self.generation == 0
                    && (self.publication != Baseline
                        || self.mutation_id.is_some()
                        || self.previous_marker_hash.is_some()))
                || (self.generation != 0
                    && (self.publication == Baseline
                        || self.mutation_id.is_none()
                        || self.previous_marker_hash.is_none()))
                || (self.publication == Leave && self.participation_hash.is_some())
                || (matches!(self.publication, Join | Write | Apply)
                    && self.participation_hash.is_none())
            {
                return Err(C12MutationErrorV1::CorruptEvidence("generation"));
            }
            serde_json::to_vec(self).map_err(|_| C12MutationErrorV1::CorruptEvidence("encode"))
        }
        pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
            let value: Self = decode_exact(bytes)?;
            value.canonical_bytes()?;
            Ok(value)
        }
        pub fn marker_hash(&self) -> Result<Hash> {
            Ok(domain_hash(
                b"polaris-sync-participation-generation-v1\0",
                &self.canonical_bytes()?,
            ))
        }
        pub fn filename(&self) -> Result<String> {
            self.canonical_bytes()?;
            Ok(format!(
                "g{:020}-{}.v1.json",
                self.generation,
                self.mutation_id
                    .map(|h| h.as_hex())
                    .unwrap_or_else(|| "baseline".into())
            ))
        }
    }

    impl C12PendingV1 {
        pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
            let expected = self
                .base_generation
                .checked_add(1)
                .ok_or(C12MutationErrorV1::Overflow)?;
            if self.schema_version != 1
                || [
                    Some(self.base_marker_hash),
                    Some(self.target_marker_hash),
                    self.target_body_hash,
                ]
                .into_iter()
                .flatten()
                .any(|hash| hash.as_hex().bytes().all(|byte| byte == b'0'))
                || self.target_marker.generation != expected
                || self.target_marker.previous_marker_hash != Some(self.base_marker_hash)
                || self.target_marker.publication != self.publication
                || self.target_marker.marker_hash()? != self.target_marker_hash
                || self.target_marker.participation_hash != self.target_body_hash
                || (self.publication == ParticipationPublicationV1::Leave
                    && (self.target_body_hash.is_some() || self.target_body_bytes != 0))
                || (self.publication != ParticipationPublicationV1::Leave
                    && (self.target_body_hash.is_none() || self.target_body_bytes == 0))
            {
                return Err(C12MutationErrorV1::CorruptEvidence("pending"));
            }
            serde_json::to_vec(self).map_err(|_| C12MutationErrorV1::CorruptEvidence("encode"))
        }
        pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
            let value: Self = decode_exact(bytes)?;
            value.canonical_bytes()?;
            Ok(value)
        }
        pub fn verify_target_body(&self, bytes: Option<&[u8]>) -> Result<()> {
            self.canonical_bytes()?;
            if bytes.map(participation_bytes_hash) != self.target_body_hash
                || bytes.map_or(0, |b| b.len() as u64) != self.target_body_bytes
            {
                return Err(C12MutationErrorV1::CorruptEvidence("target body"));
            }
            Ok(())
        }
    }

    /// Ephemeral admission components. The shared function is used before a
    /// normal mutation ID is generated, and by ApplyIntent's immutable plan.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct C12MutationCapacityV1 {
        pub pending_final_bytes: u64,
        pub body_publication_temporary_bytes: u64,
        pub generation_final_bytes: u64,
        pub generation_temporary_bytes: u64,
        pub baseline_staging_bytes: u64,
        pub directory_overhead_bytes: u64,
        pub required_bytes: u64,
    }
    impl C12MutationCapacityV1 {
        pub fn check_available(&self, available_bytes: u64) -> Result<()> {
            let required_bytes = self
                .required_bytes
                .checked_add(FREE_SPACE_FLOOR_BYTES)
                .ok_or(C12MutationErrorV1::Overflow)?;
            if available_bytes < required_bytes {
                return Err(C12MutationErrorV1::ResourceUnavailable {
                    required_bytes,
                    available_bytes,
                });
            }
            Ok(())
        }
    }

    pub fn c12_mutation_capacity_upper_bound(
        base: &ParticipationGenerationV1,
        publication: ParticipationPublicationV1,
        target_body: Option<&[u8]>,
        allocation_unit: u64,
        initializing_baseline: bool,
    ) -> Result<C12MutationCapacityV1> {
        let base_hash = base.marker_hash()?;
        if publication == ParticipationPublicationV1::Baseline
            || (publication == ParticipationPublicationV1::Leave) != target_body.is_none()
        {
            return Err(C12MutationErrorV1::CorruptEvidence("publication/body"));
        }
        let placeholder =
            Hash::from_hex("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
                .expect("fixed-width capacity hash");
        let target_marker = ParticipationGenerationV1 {
            schema_version: 1,
            generation: base
                .generation
                .checked_add(1)
                .ok_or(C12MutationErrorV1::Overflow)?,
            mutation_id: Some(placeholder),
            participation_hash: target_body.map(participation_bytes_hash),
            previous_marker_hash: Some(base_hash),
            publication,
        };
        let pending = C12PendingV1 {
            schema_version: 1,
            base_generation: base.generation,
            base_marker_hash: base_hash,
            target_marker_hash: target_marker.marker_hash()?,
            target_marker,
            target_body_hash: target_body.map(participation_bytes_hash),
            target_body_bytes: target_body.map_or(0, |b| b.len() as u64),
            publication,
        };
        let alloc =
            |n| allocated_bytes(n, allocation_unit).map_err(|_| C12MutationErrorV1::Overflow);
        let sum = |values: &[u64]| {
            values.iter().try_fold(0u64, |n, v| {
                n.checked_add(*v).ok_or(C12MutationErrorV1::Overflow)
            })
        };
        let pending_final_bytes = sum(&[
            alloc(pending.canonical_bytes()?.len() as u64)?,
            alloc(pending.target_body_bytes)?,
            DIRECTORY_OVERHEAD_BYTES,
        ])?;
        let body_publication_temporary_bytes = alloc(pending.target_body_bytes)?;
        let generation_final_bytes = alloc(pending.target_marker.canonical_bytes()?.len() as u64)?;
        let generation_temporary_bytes = generation_final_bytes;
        let baseline_staging_bytes = if initializing_baseline {
            let baseline = ParticipationGenerationV1 {
                schema_version: 1,
                generation: 0,
                mutation_id: None,
                participation_hash: Some(placeholder),
                previous_marker_hash: None,
                publication: ParticipationPublicationV1::Baseline,
            };
            let record = BaselineV1 {
                schema_version: 1,
                baseline_marker_hash: baseline.marker_hash()?,
            };
            sum(&[
                DIRECTORY_OVERHEAD_BYTES,
                DIRECTORY_OVERHEAD_BYTES,
                alloc(baseline.canonical_bytes()?.len() as u64)?,
                alloc(
                    serde_json::to_vec(&record)
                        .map_err(|_| C12MutationErrorV1::CorruptEvidence("baseline encode"))?
                        .len() as u64,
                )?,
            ])?
        } else {
            0
        };
        let required_bytes = sum(&[
            pending_final_bytes,
            body_publication_temporary_bytes,
            generation_final_bytes,
            generation_temporary_bytes,
            baseline_staging_bytes,
            DIRECTORY_OVERHEAD_BYTES,
        ])?;
        Ok(C12MutationCapacityV1 {
            pending_final_bytes,
            body_publication_temporary_bytes,
            generation_final_bytes,
            generation_temporary_bytes,
            baseline_staging_bytes,
            directory_overhead_bytes: DIRECTORY_OVERHEAD_BYTES,
            required_bytes,
        })
    }

    use crate::project_mutation::ProjectMutationLeaseV1;
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::CString,
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
        path::Component,
    };

    const GENERATION_ROOT: &str = "participation-generations-v1";
    const BASELINE_FILE: &str = "baseline.v1.json";
    const PENDING_DIR: &str = "pending.v1";
    const GENERATIONS_DIR: &str = "generations";

    /// A pending record is evidence, not authority to change participation.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum C12PendingPositionV1 {
        Base,
        Target,
        Committed,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum C12InspectionV1 {
        Legacy {
            participation_hash: Option<Hash>,
        },
        Committed {
            marker: ParticipationGenerationV1,
            pending: Option<(C12PendingV1, C12PendingPositionV1)>,
        },
        /// Only an empty pending directory remains after committed cleanup.
        CleanupCommitted {
            marker: ParticipationGenerationV1,
        },
    }

    #[derive(Debug)]
    struct Directory {
        file: File,
        path: PathBuf,
        chain: Vec<(u64, u64)>,
        private: bool,
    }

    fn descriptor(fd: libc::c_int) -> Result<File> {
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful open/openat transfers a fresh owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn component(name: &str) -> Result<CString> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(C12MutationErrorV1::CorruptEvidence("component"));
        }
        CString::new(name).map_err(|_| C12MutationErrorV1::CorruptEvidence("component"))
    }

    fn identity(meta: &fs::Metadata) -> (u64, u64) {
        (meta.dev(), meta.ino())
    }

    impl Directory {
        fn open(path: &Path, private: bool) -> Result<Self> {
            if !path.is_absolute() {
                return Err(C12MutationErrorV1::CorruptEvidence("absolute root"));
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open("/")?;
            let mut chain = vec![identity(&file.metadata()?)];
            for part in path.components() {
                match part {
                    Component::RootDir => (),
                    Component::Normal(name) => {
                        let name = name
                            .to_str()
                            .ok_or(C12MutationErrorV1::CorruptEvidence("UTF-8 root"))?;
                        let name = component(name)?;
                        file = descriptor(unsafe {
                            libc::openat(
                                file.as_raw_fd(),
                                name.as_ptr(),
                                libc::O_RDONLY
                                    | libc::O_DIRECTORY
                                    | libc::O_NOFOLLOW
                                    | libc::O_CLOEXEC,
                            )
                        })?;
                        chain.push(identity(&file.metadata()?));
                    }
                    _ => return Err(C12MutationErrorV1::CorruptEvidence("root traversal")),
                }
            }
            let meta = file.metadata()?;
            if meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o7022 != 0
                || (private && meta.mode() & 0o777 != 0o700)
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "directory ownership/mode",
                ));
            }
            Ok(Self {
                file,
                path: path.to_path_buf(),
                chain,
                private,
            })
        }

        fn verify(&self) -> Result<()> {
            let current = Self::open(&self.path, self.private)?;
            if current.chain != self.chain
                || identity(&self.file.metadata()?) != *self.chain.last().expect("root")
            {
                return Err(C12MutationErrorV1::CorruptEvidence("directory raced"));
            }
            Ok(())
        }

        fn child(&self, name: &str, private: bool) -> Result<Option<Self>> {
            self.verify()?;
            let name_c = component(name)?;
            let file = match descriptor(unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            }) {
                Ok(file) => file,
                Err(C12MutationErrorV1::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            let child = Self::open(&self.path.join(name), private)?;
            if identity(&file.metadata()?) != identity(&child.file.metadata()?) {
                return Err(C12MutationErrorV1::CorruptEvidence("child raced"));
            }
            self.verify()?;
            Ok(Some(child))
        }

        fn create_child(&self, name: &str) -> Result<Self> {
            self.verify()?;
            let name_c = component(name)?;
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
            self.child(name, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence(
                    "missing created directory",
                ))
        }

        fn names(&self) -> Result<Vec<String>> {
            self.verify()?;
            let mut names = Vec::new();
            for entry in fs::read_dir(&self.path)? {
                if names.len() == 3 {
                    return Err(C12MutationErrorV1::CorruptEvidence("foreign control entry"));
                }
                names.push(
                    entry?
                        .file_name()
                        .into_string()
                        .map_err(|_| C12MutationErrorV1::CorruptEvidence("non-UTF-8 C12 entry"))?,
                );
            }
            self.verify()?;
            Ok(names)
        }

        fn read(&self, name: &str, maximum: Option<u64>, private: bool) -> Result<Option<Vec<u8>>> {
            self.verify()?;
            let name = component(name)?;
            let mut file = match descriptor(unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            }) {
                Ok(file) => file,
                Err(C12MutationErrorV1::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            let before = file.metadata()?;
            if !before.is_file()
                || before.nlink() != 1
                || before.uid() != unsafe { libc::geteuid() }
                || before.mode() & 0o7022 != 0
                || (private && before.mode() & 0o777 != 0o600)
                || maximum.is_some_and(|limit| before.len() > limit)
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "file ownership/type/length",
                ));
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(
                    usize::try_from(before.len()).map_err(|_| C12MutationErrorV1::Overflow)?,
                )
                .map_err(|_| C12MutationErrorV1::Overflow)?;
            Read::by_ref(&mut file)
                .take(
                    before
                        .len()
                        .checked_add(1)
                        .ok_or(C12MutationErrorV1::Overflow)?,
                )
                .read_to_end(&mut bytes)?;
            let after = file.metadata()?;
            let named = descriptor(unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            })?
            .metadata()?;
            if bytes.len() as u64 != before.len()
                || identity(&before) != identity(&after)
                || identity(&before) != identity(&named)
                || after.len() != before.len()
                || after.mtime() != before.mtime()
                || after.mtime_nsec() != before.mtime_nsec()
                || after.ctime() != before.ctime()
                || after.ctime_nsec() != before.ctime_nsec()
                || named.len() != after.len()
                || named.uid() != after.uid()
                || named.mode() != after.mode()
                || named.nlink() != after.nlink()
                || named.ctime() != after.ctime()
                || named.ctime_nsec() != after.ctime_nsec()
            {
                return Err(C12MutationErrorV1::CorruptEvidence("file raced"));
            }
            self.verify()?;
            Ok(Some(bytes))
        }

        fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
            self.verify()?;
            let name_c = component(name)?;
            let mut file = descriptor(unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDWR
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_CREAT
                        | libc::O_EXCL,
                    0o600,
                )
            })?;
            let initial = file.metadata()?;
            file.write_all(bytes)?;
            file.sync_data()?;
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0))?;
            let mut actual = Vec::new();
            file.read_to_end(&mut actual)?;
            if actual != bytes
                || self.read(name, Some(bytes.len() as u64), true)?.as_deref() != Some(bytes)
                || identity(&initial) != identity(&file.metadata()?)
                || identity(&initial) != identity(&fs::symlink_metadata(self.path.join(name))?)
            {
                return Err(C12MutationErrorV1::CorruptEvidence("publication readback"));
            }
            self.file.sync_all()?;
            self.verify()
        }

        fn publish_directory(&self, staging: &Directory, target: &str) -> Result<()> {
            self.verify()?;
            staging.verify()?;
            let from = component(
                staging
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(C12MutationErrorV1::CorruptEvidence("staging name"))?,
            )?;
            let to = component(target)?;
            #[cfg(target_os = "linux")]
            let result = unsafe {
                libc::renameat2(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    self.file.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let result = unsafe {
                libc::renameatx_np(
                    self.file.as_raw_fd(),
                    from.as_ptr(),
                    self.file.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
            let published = self
                .child(target, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing publication"))?;
            if identity(&published.file.metadata()?) != identity(&staging.file.metadata()?) {
                return Err(C12MutationErrorV1::CorruptEvidence("publication identity"));
            }
            self.verify()
        }

        fn capacity(&self) -> Result<(u64, u64)> {
            self.verify()?;
            let mut raw = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            if unsafe { libc::fstatvfs(self.file.as_raw_fd(), raw.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            let raw = unsafe { raw.assume_init() };
            let unit = raw.f_frsize as u64;
            if unit == 0 {
                return Err(C12MutationErrorV1::CorruptEvidence("allocation unit"));
            }
            let available = (raw.f_bavail as u64)
                .checked_mul(unit)
                .ok_or(C12MutationErrorV1::Overflow)?;
            self.verify()?;
            Ok((unit, available))
        }
    }

    /// The project lease stays borrowed throughout inspection and publication.
    pub(crate) struct C12GenerationOwnerV1<'a> {
        state: Directory,
        _lease: &'a ProjectMutationLeaseV1,
        #[cfg(test)]
        fault: std::cell::Cell<Option<(&'static str, i32)>>,
        #[cfg(test)]
        capacity_override: std::cell::Cell<Option<(u64, u64)>>,
    }

    impl<'a> C12GenerationOwnerV1<'a> {
        pub(crate) fn open(state: &Path, lease: &'a ProjectMutationLeaseV1) -> Result<Self> {
            let state = Directory::open(state, false)?;
            let lock = state.child("rewind-workspace", true)?.ok_or(
                C12MutationErrorV1::CorruptEvidence("missing project lease root"),
            )?;
            if lock.path != lease.store_root()
                || identity(&lock.file.metadata()?) != lease.store_identity()
            {
                return Err(C12MutationErrorV1::CorruptEvidence("foreign project lease"));
            }
            Ok(Self {
                state,
                _lease: lease,
                #[cfg(test)]
                fault: std::cell::Cell::new(None),
                #[cfg(test)]
                capacity_override: std::cell::Cell::new(None),
            })
        }

        fn checkpoint(&self, stage: &'static str) -> Result<()> {
            #[cfg(test)]
            if let Some((expected, errno)) = self.fault.get()
                && expected == stage
            {
                self.fault.set(None);
                return Err(io::Error::from_raw_os_error(errno).into());
            }
            let _ = stage;
            Ok(())
        }

        fn capacity(&self, directory: &Directory) -> Result<(u64, u64)> {
            let actual = directory.capacity()?;
            #[cfg(test)]
            if let Some(value) = self.capacity_override.get() {
                return Ok(value);
            }
            Ok(actual)
        }

        #[cfg(test)]
        pub(super) fn inject_failure(&self, stage: &'static str, errno: i32) {
            self.fault.set(Some((stage, errno)));
        }

        #[cfg(test)]
        pub(super) fn set_capacity(&self, capacity: Option<(u64, u64)>) {
            self.capacity_override.set(capacity);
        }

        fn sync(&self) -> Result<Option<Directory>> {
            self.state.child("sync", false)
        }

        pub(crate) fn inspect(&self) -> Result<C12InspectionV1> {
            let Some(sync) = self.sync()? else {
                return Ok(C12InspectionV1::Legacy {
                    participation_hash: None,
                });
            };
            let current_body = sync.read(FILE, None, false)?;
            let body_hash = current_body.as_deref().map(participation_bytes_hash);
            let Some(root) = sync.child(GENERATION_ROOT, true)? else {
                return Ok(C12InspectionV1::Legacy {
                    participation_hash: body_hash,
                });
            };
            for name in root.names()? {
                if !matches!(name.as_str(), BASELINE_FILE | GENERATIONS_DIR | PENDING_DIR) {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "foreign generation root entry",
                    ));
                }
            }
            let baseline: BaselineV1 = decode_exact(
                &root
                    .read(BASELINE_FILE, Some(65536), true)?
                    .ok_or(C12MutationErrorV1::CorruptEvidence("missing baseline"))?,
            )?;
            if baseline.schema_version != 1 {
                return Err(C12MutationErrorV1::CorruptEvidence("baseline version"));
            }
            let generations = root
                .child(GENERATIONS_DIR, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing generations"))?;
            // Decode each immutable record separately; retain no artifact bodies.
            let mut records = BTreeMap::new();
            let mut mutation_ids = BTreeSet::new();
            let mut generation_temporary = None;
            generations.verify()?;
            for entry in fs::read_dir(&generations.path)? {
                let name = entry?
                    .file_name()
                    .into_string()
                    .map_err(|_| C12MutationErrorV1::CorruptEvidence("generation filename"))?;
                if name.starts_with('.') && name.ends_with(".tmp") {
                    if generation_temporary.is_some() {
                        return Err(C12MutationErrorV1::CorruptEvidence(
                            "multiple generation temporaries",
                        ));
                    }
                    let bytes = generations.read(&name, Some(65536), true)?.ok_or(
                        C12MutationErrorV1::CorruptEvidence("missing generation temporary"),
                    )?;
                    generation_temporary = Some((name, bytes));
                    continue;
                }
                let marker = ParticipationGenerationV1::from_canonical_bytes(
                    &generations
                        .read(&name, Some(65536), true)?
                        .ok_or(C12MutationErrorV1::CorruptEvidence("missing generation"))?,
                )?;
                if marker
                    .mutation_id
                    .is_some_and(|id| !mutation_ids.insert(id))
                {
                    return Err(C12MutationErrorV1::CorruptEvidence("reused mutation ID"));
                }
                if marker.filename()? != name || records.insert(marker.generation, marker).is_some()
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "generation filename/duplicate",
                    ));
                }
            }
            generations.verify()?;
            let first = records.get(&0).ok_or(C12MutationErrorV1::CorruptEvidence(
                "missing generation zero",
            ))?;
            if first.marker_hash()? != baseline.baseline_marker_hash {
                return Err(C12MutationErrorV1::CorruptEvidence("baseline hash"));
            }
            let mut previous = None;
            let mut expected = 0u64;
            for (number, marker) in &records {
                if *number != expected || marker.previous_marker_hash != previous {
                    return Err(C12MutationErrorV1::CorruptEvidence("generation gap/fork"));
                }
                previous = Some(marker.marker_hash()?);
                expected = number.checked_add(1).unwrap_or(*number);
            }
            let marker = records
                .last_key_value()
                .expect("generation zero exists")
                .1
                .clone();
            let pending = if let Some(directory) = root.child(PENDING_DIR, true)? {
                let names = directory.names()?;
                if names.is_empty()
                    && marker.generation > 0
                    && body_hash == marker.participation_hash
                    && generation_temporary.is_none()
                {
                    root.verify()?;
                    sync.verify()?;
                    if sync.read(FILE, None, false)? != current_body {
                        return Err(C12MutationErrorV1::CorruptEvidence(
                            "body raced during inspection",
                        ));
                    }
                    return Ok(C12InspectionV1::CleanupCommitted { marker });
                }
                let pending = C12PendingV1::from_canonical_bytes(
                    &directory
                        .read("operation.v1.json", Some(65536), true)?
                        .ok_or(C12MutationErrorV1::CorruptEvidence(
                            "missing pending operation",
                        ))?,
                )?;
                if names
                    .iter()
                    .any(|n| n != "operation.v1.json" && n != "target-body.v1.json")
                {
                    return Err(C12MutationErrorV1::CorruptEvidence("foreign pending entry"));
                }
                let target = directory.read("target-body.v1.json", None, true)?;
                if target.is_none()
                    && marker == pending.target_marker
                    && body_hash == pending.target_body_hash
                {
                    // Cleanup may already have removed the retained copy;
                    // the authoritative generation and current exact body
                    // still prove that only cleanup remains.
                    pending.verify_target_body(current_body.as_deref())?;
                } else {
                    pending.verify_target_body(target.as_deref())?;
                }
                let base = records
                    .get(&pending.base_generation)
                    .ok_or(C12MutationErrorV1::CorruptEvidence("pending base missing"))?;
                if base.marker_hash()? != pending.base_marker_hash {
                    return Err(C12MutationErrorV1::CorruptEvidence("pending base mismatch"));
                }
                if marker != pending.target_marker
                    && mutation_ids.contains(&pending.target_marker.mutation_id.expect("validated"))
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "pending mutation ID reused",
                    ));
                }
                let position =
                    if marker == pending.target_marker && body_hash == pending.target_body_hash {
                        C12PendingPositionV1::Committed
                    } else if marker == *base && body_hash == base.participation_hash {
                        C12PendingPositionV1::Base
                    } else if marker == *base && body_hash == pending.target_body_hash {
                        C12PendingPositionV1::Target
                    } else {
                        return Err(C12MutationErrorV1::CorruptEvidence(
                            "pending rollback/conflict",
                        ));
                    };
                Some((pending, position))
            } else {
                if body_hash != marker.participation_hash {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "participation rollback",
                    ));
                }
                None
            };
            if let Some((name, bytes)) = generation_temporary {
                let Some((pending, _)) = pending.as_ref() else {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "unowned generation temporary",
                    ));
                };
                if name != format!(".{}.tmp", pending.target_marker.filename()?)
                    || !pending.target_marker.canonical_bytes()?.starts_with(&bytes)
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "foreign generation temporary",
                    ));
                }
            }
            root.verify()?;
            sync.verify()?;
            if sync.read(FILE, None, false)? != current_body {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "body raced during inspection",
                ));
            }
            Ok(C12InspectionV1::Committed { marker, pending })
        }

        pub(crate) fn initialize_for_mutation(
            &self,
            publication: ParticipationPublicationV1,
            body: Option<&[u8]>,
        ) -> Result<ParticipationGenerationV1> {
            let C12InspectionV1::Legacy { participation_hash } = self.inspect()? else {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "baseline already exists",
                ));
            };
            let base = ParticipationGenerationV1 {
                schema_version: 1,
                generation: 0,
                mutation_id: None,
                participation_hash,
                previous_marker_hash: None,
                publication: ParticipationPublicationV1::Baseline,
            };
            let sync = self.sync()?;
            let parent = sync.as_ref().unwrap_or(&self.state);
            let parent_device = parent.file.metadata()?.dev();
            let (unit, available) = self.capacity(parent)?;
            c12_mutation_capacity_upper_bound(&base, publication, body, unit, true)?
                .check_available(available)?;
            // Capacity admission precedes randomness and every C12 directory/file.
            let mut nonce = [0u8; 12];
            getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
            let name = format!(
                ".participation-generations-v1.{}.tmp",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            let sync = match sync {
                Some(sync) => sync,
                None => self.state.create_child("sync")?,
            };
            if sync.file.metadata()?.dev() != parent_device {
                return Err(C12MutationErrorV1::CorruptEvidence("sync device changed"));
            }
            if sync
                .read(FILE, None, false)?
                .as_deref()
                .map(participation_bytes_hash)
                != participation_hash
                || sync.child(GENERATION_ROOT, true)?.is_some()
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "baseline source changed",
                ));
            }
            let staging = sync.create_child(&name)?;
            let generations = staging.create_child(GENERATIONS_DIR)?;
            self.checkpoint("baseline-marker")?;
            generations.write_new(&base.filename()?, &base.canonical_bytes()?)?;
            let record = BaselineV1 {
                schema_version: 1,
                baseline_marker_hash: base.marker_hash()?,
            };
            self.checkpoint("baseline-record")?;
            staging.write_new(
                BASELINE_FILE,
                &serde_json::to_vec(&record)
                    .map_err(|_| C12MutationErrorV1::CorruptEvidence("baseline encode"))?,
            )?;
            generations.file.sync_all()?;
            staging.file.sync_all()?;
            if sync
                .read(FILE, None, false)?
                .as_deref()
                .map(participation_bytes_hash)
                != participation_hash
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "baseline source changed",
                ));
            }
            self.checkpoint("baseline-publish")?;
            sync.publish_directory(&staging, GENERATION_ROOT)?;
            self.inspect()?;
            Ok(base)
        }
    }

    impl Directory {
        fn unlink_exact(&self, name: &str, expected: &[u8]) -> Result<()> {
            self.verify()?;
            let path = self.path.join(name);
            let before = fs::symlink_metadata(&path)?;
            if self
                .read(name, Some(expected.len() as u64), true)?
                .as_deref()
                != Some(expected)
            {
                return Err(C12MutationErrorV1::CorruptEvidence("cleanup bytes"));
            }
            let after = fs::symlink_metadata(&path)?;
            if identity(&before) != identity(&after) {
                return Err(C12MutationErrorV1::CorruptEvidence("cleanup raced"));
            }
            let name = component(name)?;
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
            self.verify()
        }

        fn remove_empty_child(&self, directory: &Directory) -> Result<()> {
            self.verify()?;
            directory.verify()?;
            if !directory.names()?.is_empty() {
                return Err(C12MutationErrorV1::CorruptEvidence("cleanup nonempty"));
            }
            let name = directory
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or(C12MutationErrorV1::CorruptEvidence("cleanup child name"))?;
            let current = self
                .child(name, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("cleanup missing child"))?;
            if identity(&current.file.metadata()?) != identity(&directory.file.metadata()?) {
                return Err(C12MutationErrorV1::CorruptEvidence("cleanup child raced"));
            }
            let name = component(name)?;
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
            self.verify()
        }

        fn rename_file(&self, from: &str, to: &str, replace: bool) -> Result<()> {
            self.verify()?;
            let from = component(from)?;
            let to = component(to)?;
            let result = if replace {
                unsafe {
                    libc::renameat(
                        self.file.as_raw_fd(),
                        from.as_ptr(),
                        self.file.as_raw_fd(),
                        to.as_ptr(),
                    )
                }
            } else {
                #[cfg(target_os = "linux")]
                {
                    unsafe {
                        libc::renameat2(
                            self.file.as_raw_fd(),
                            from.as_ptr(),
                            self.file.as_raw_fd(),
                            to.as_ptr(),
                            libc::RENAME_NOREPLACE,
                        )
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    unsafe {
                        libc::renameatx_np(
                            self.file.as_raw_fd(),
                            from.as_ptr(),
                            self.file.as_raw_fd(),
                            to.as_ptr(),
                            libc::RENAME_EXCL,
                        )
                    }
                }
            };
            if result != 0 {
                return Err(io::Error::last_os_error().into());
            }
            self.file.sync_all()?;
            self.verify()
        }

        // A named temporary belongs to the pending operation only if its exact
        // bytes are a prefix of that operation's immutable expected payload.
        fn prepare_temporary(&self, name: &str, bytes: &[u8]) -> Result<()> {
            if let Some(existing) = self.read(name, Some(bytes.len() as u64), true)? {
                if !bytes.starts_with(&existing) {
                    return Err(C12MutationErrorV1::CorruptEvidence("foreign temporary"));
                }
                self.unlink_exact(name, &existing)?;
            }
            self.write_new(name, bytes)
        }
    }

    fn pending_from(
        base: &ParticipationGenerationV1,
        publication: ParticipationPublicationV1,
        body: Option<&[u8]>,
        mutation_id: Hash,
    ) -> Result<C12PendingV1> {
        if publication == ParticipationPublicationV1::Baseline
            || (publication == ParticipationPublicationV1::Leave) != body.is_none()
        {
            return Err(C12MutationErrorV1::CorruptEvidence("publication/body"));
        }
        let target_marker = ParticipationGenerationV1 {
            schema_version: 1,
            generation: base
                .generation
                .checked_add(1)
                .ok_or(C12MutationErrorV1::Overflow)?,
            mutation_id: Some(mutation_id),
            participation_hash: body.map(participation_bytes_hash),
            previous_marker_hash: Some(base.marker_hash()?),
            publication,
        };
        let pending = C12PendingV1 {
            schema_version: 1,
            base_generation: base.generation,
            base_marker_hash: base.marker_hash()?,
            target_marker_hash: target_marker.marker_hash()?,
            target_marker,
            target_body_hash: body.map(participation_bytes_hash),
            target_body_bytes: body.map_or(0, |b| b.len() as u64),
            publication,
        };
        pending.verify_target_body(body)?;
        Ok(pending)
    }

    fn normal_mutation_id(
        base_hash: Hash,
        publication: ParticipationPublicationV1,
    ) -> Result<Hash> {
        let tag = match publication {
            ParticipationPublicationV1::Join => 1u8,
            ParticipationPublicationV1::Write => 2,
            ParticipationPublicationV1::Leave => 3,
            _ => return Err(C12MutationErrorV1::CorruptEvidence("normal publication")),
        };
        let mut nonce = [0u8; 32];
        getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
        let hex = base_hash.as_hex();
        let mut raw = [0u8; 32];
        for (i, byte) in raw.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| C12MutationErrorV1::CorruptEvidence("base hash"))?;
        }
        let mut bytes = Vec::with_capacity(95);
        bytes.extend_from_slice(b"polaris-c12-mutation-v1\0");
        bytes.extend_from_slice(&raw);
        bytes.push(tag);
        bytes.extend_from_slice(&nonce);
        Ok(Hash::digest(&bytes))
    }

    impl C12GenerationOwnerV1<'_> {
        pub(crate) fn current_body(&self) -> Result<Option<Vec<u8>>> {
            self.inspect()?;
            match self.sync()? {
                Some(sync) => sync.read(FILE, None, false),
                None => Ok(None),
            }
        }

        fn publish_pending(
            &self,
            base: &ParticipationGenerationV1,
            publication: ParticipationPublicationV1,
            body: Option<&[u8]>,
            mutation_id: Hash,
        ) -> Result<C12PendingV1> {
            if self.inspect()?
                != (C12InspectionV1::Committed {
                    marker: base.clone(),
                    pending: None,
                })
            {
                return Err(C12MutationErrorV1::CorruptEvidence("pending base raced"));
            }
            let pending = pending_from(base, publication, body, mutation_id)?;
            let sync = self
                .sync()?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
            let root =
                sync.child(GENERATION_ROOT, true)?
                    .ok_or(C12MutationErrorV1::CorruptEvidence(
                        "missing generation root",
                    ))?;
            let generations = root
                .child(GENERATIONS_DIR, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing generations"))?;
            let suffix = format!("-{}.v1.json", mutation_id.as_hex());
            for entry in fs::read_dir(&generations.path)? {
                if entry?
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(&suffix))
                {
                    return Err(C12MutationErrorV1::CorruptEvidence("mutation ID collision"));
                }
            }
            generations.verify()?;
            let mut nonce = [0u8; 12];
            getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
            let name = format!(
                ".pending-v1.{}.tmp",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            let staging = root.create_child(&name)?;
            let operation = pending.canonical_bytes()?;
            let staged = (|| {
                self.checkpoint("pending-control")?;
                staging.write_new("operation.v1.json", &operation)?;
                if let Some(body) = body {
                    self.checkpoint("pending-target")?;
                    staging.write_new("target-body.v1.json", body)?;
                }
                staging.file.sync_all()?;
                // The owned staging is the only additional control entry here.
                if sync
                    .read(FILE, None, false)?
                    .as_deref()
                    .map(participation_bytes_hash)
                    != base.participation_hash
                {
                    return Err(C12MutationErrorV1::CorruptEvidence("pending source raced"));
                }
                self.checkpoint("pending-publish")?;
                root.publish_directory(&staging, PENDING_DIR)?;
                sync.file.sync_all()?;
                Ok(())
            })();
            if let Err(error) = staged {
                // Cleanup is allowed only before publication, for these exact
                // owned entries, while the current body still equals the base.
                if root.child(PENDING_DIR, true)?.is_none()
                    && sync
                        .read(FILE, None, false)?
                        .as_deref()
                        .map(participation_bytes_hash)
                        == base.participation_hash
                {
                    for (name, expected) in [
                        ("operation.v1.json", Some(operation.as_slice())),
                        ("target-body.v1.json", body),
                    ] {
                        if let Some(actual) =
                            staging.read(name, expected.map(|b| b.len() as u64), true)?
                            && expected.is_some_and(|bytes| bytes.starts_with(&actual))
                        {
                            staging.unlink_exact(name, &actual)?;
                        }
                    }
                    root.remove_empty_child(&staging)?;
                }
                return Err(error);
            }
            self.inspect()?;
            Ok(pending)
        }

        pub(crate) fn publish_normal(
            &self,
            publication: ParticipationPublicationV1,
            body: Option<&[u8]>,
        ) -> Result<ParticipationGenerationV1> {
            if !matches!(
                publication,
                ParticipationPublicationV1::Join
                    | ParticipationPublicationV1::Write
                    | ParticipationPublicationV1::Leave
            ) {
                return Err(C12MutationErrorV1::CorruptEvidence("normal publication"));
            }
            if matches!(self.inspect()?, C12InspectionV1::CleanupCommitted { .. }) {
                self.resume_pending()?;
            }
            if let C12InspectionV1::Committed {
                pending: Some((pending, _)),
                ..
            } = self.inspect()?
            {
                if pending.publication != publication
                    || pending.target_body_hash != body.map(participation_bytes_hash)
                    || pending.target_body_bytes != body.map_or(0, |b| b.len() as u64)
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "different pending mutation",
                    ));
                }
                return self.resume_pending();
            }
            if matches!(self.inspect()?, C12InspectionV1::Legacy { .. }) {
                self.initialize_for_mutation(publication, body)?;
            }
            let C12InspectionV1::Committed {
                marker: base,
                pending: None,
            } = self.inspect()?
            else {
                return Err(C12MutationErrorV1::CorruptEvidence("pending mutation"));
            };
            let sync = self
                .sync()?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
            let (unit, available) = self.capacity(&sync)?;
            c12_mutation_capacity_upper_bound(&base, publication, body, unit, false)?
                .check_available(available)?;
            let mutation_id = normal_mutation_id(base.marker_hash()?, publication)?;
            self.publish_pending(&base, publication, body, mutation_id)?;
            self.resume_pending()
        }

        /// F5 calls this only after binding the descriptor, reserved capacity
        /// and effect_started under the same borrowed project mutation lease.
        pub(crate) fn publish_apply_pending(
            &self,
            base: &ParticipationGenerationV1,
            body: &[u8],
            operation_key: Hash,
        ) -> Result<C12PendingV1> {
            self.publish_pending(
                base,
                ParticipationPublicationV1::Apply,
                Some(body),
                operation_key,
            )
        }

        /// Restart uses the existing pending ID and exact retained body.
        /// It does not perform a new normal-mutation admission.
        pub(crate) fn resume_pending(&self) -> Result<ParticipationGenerationV1> {
            let inspected = self.inspect()?;
            if let C12InspectionV1::CleanupCommitted { marker } = inspected {
                let sync = self
                    .sync()?
                    .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
                let root = sync.child(GENERATION_ROOT, true)?.ok_or(
                    C12MutationErrorV1::CorruptEvidence("missing generation root"),
                )?;
                let directory =
                    root.child(PENDING_DIR, true)?
                        .ok_or(C12MutationErrorV1::CorruptEvidence(
                            "missing cleanup directory",
                        ))?;
                root.remove_empty_child(&directory)?;
                sync.file.sync_all()?;
                return Ok(marker);
            }
            let C12InspectionV1::Committed {
                marker,
                pending: Some((pending, position)),
            } = inspected
            else {
                return Err(C12MutationErrorV1::CorruptEvidence("no published pending"));
            };
            let sync = self
                .sync()?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
            let root =
                sync.child(GENERATION_ROOT, true)?
                    .ok_or(C12MutationErrorV1::CorruptEvidence(
                        "missing generation root",
                    ))?;
            let directory = root
                .child(PENDING_DIR, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing pending"))?;
            let retained_body = directory.read("target-body.v1.json", None, true)?;
            let body = if position == C12PendingPositionV1::Committed && retained_body.is_none() {
                sync.read(FILE, None, false)?
            } else {
                retained_body.clone()
            };
            pending.verify_target_body(body.as_deref())?;
            let generations = root
                .child(GENERATIONS_DIR, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing generations"))?;
            let (unit, available) = self.capacity(&sync)?;
            if position != C12PendingPositionV1::Committed {
                let mut remaining = c12_mutation_capacity_upper_bound(
                    &marker,
                    pending.publication,
                    body.as_deref(),
                    unit,
                    false,
                )?;
                remaining.required_bytes = remaining
                    .generation_final_bytes
                    .checked_add(remaining.generation_temporary_bytes)
                    .and_then(|n| n.checked_add(remaining.directory_overhead_bytes))
                    .ok_or(C12MutationErrorV1::Overflow)?;
                if position == C12PendingPositionV1::Base {
                    remaining.required_bytes = remaining
                        .required_bytes
                        .checked_add(remaining.body_publication_temporary_bytes)
                        .ok_or(C12MutationErrorV1::Overflow)?;
                }
                remaining.check_available(available)?;
            }
            if position == C12PendingPositionV1::Base {
                if sync
                    .read(FILE, None, false)?
                    .as_deref()
                    .map(participation_bytes_hash)
                    != marker.participation_hash
                {
                    return Err(C12MutationErrorV1::CorruptEvidence("base body changed"));
                }
                if let Some(body) = body.as_deref() {
                    let temporary = format!(
                        ".participation-{}.tmp",
                        pending
                            .target_marker
                            .mutation_id
                            .expect("validated")
                            .as_hex()
                    );
                    self.checkpoint("body-temporary")?;
                    sync.prepare_temporary(&temporary, body)?;
                    if sync
                        .read(FILE, None, false)?
                        .as_deref()
                        .map(participation_bytes_hash)
                        != marker.participation_hash
                    {
                        return Err(C12MutationErrorV1::CorruptEvidence("base body changed"));
                    }
                    self.checkpoint("body-publish")?;
                    sync.rename_file(&temporary, FILE, true)?;
                    if sync.read(FILE, Some(body.len() as u64), true)?.as_deref() != Some(body) {
                        return Err(C12MutationErrorV1::CorruptEvidence("target publication"));
                    }
                } else if let Some(base_body) = sync.read(FILE, None, false)? {
                    // Legacy baselines can retain 0644 files; unlink verifies
                    // exact bytes and identity without changing their mode.
                    self.checkpoint("body-publish")?;
                    self.unlink_participation(&sync, &base_body)?;
                }
                self.checkpoint("body-sync")?;
                sync.file.sync_all()?;
            }
            if position != C12PendingPositionV1::Committed {
                if sync
                    .read(FILE, None, false)?
                    .as_deref()
                    .map(participation_bytes_hash)
                    != pending.target_body_hash
                {
                    return Err(C12MutationErrorV1::CorruptEvidence("target body changed"));
                }
                let bytes = pending.target_marker.canonical_bytes()?;
                let name = pending.target_marker.filename()?;
                let temporary = format!(".{name}.tmp");
                self.checkpoint("generation-temporary")?;
                generations.prepare_temporary(&temporary, &bytes)?;
                self.checkpoint("generation-publish")?;
                generations.rename_file(&temporary, &name, false)?;
                if generations
                    .read(&name, Some(bytes.len() as u64), true)?
                    .as_deref()
                    != Some(bytes.as_slice())
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "generation publication",
                    ));
                }
                self.checkpoint("generation-sync")?;
                sync.file.sync_all()?;
            }
            // The immutable generation is now authoritative. Keep the control
            // file until the target body has been removed from pending.
            if let Some(body) = retained_body.as_deref() {
                self.checkpoint("cleanup-body")?;
                directory.unlink_exact("target-body.v1.json", body)?;
            }
            self.checkpoint("cleanup-operation")?;
            directory.unlink_exact("operation.v1.json", &pending.canonical_bytes()?)?;
            self.checkpoint("cleanup-directory")?;
            root.remove_empty_child(&directory)?;
            sync.file.sync_all()?;
            let final_state = self.inspect()?;
            if final_state
                != (C12InspectionV1::Committed {
                    marker: pending.target_marker.clone(),
                    pending: None,
                })
            {
                return Err(C12MutationErrorV1::CorruptEvidence("committed readback"));
            }
            Ok(pending.target_marker)
        }

        fn unlink_participation(&self, sync: &Directory, expected: &[u8]) -> Result<()> {
            sync.verify()?;
            let before = fs::symlink_metadata(sync.path.join(FILE))?;
            if sync
                .read(FILE, Some(expected.len() as u64), false)?
                .as_deref()
                != Some(expected)
                || identity(&before) != identity(&fs::symlink_metadata(sync.path.join(FILE))?)
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "participation unlink raced",
                ));
            }
            let name = component(FILE)?;
            if unsafe { libc::unlinkat(sync.file.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            sync.file.sync_all()?;
            sync.verify()
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use generations_v1::{
    C12MutationCapacityV1, C12MutationErrorV1, C12PendingV1, ParticipationGenerationV1,
    ParticipationPublicationV1, c12_mutation_capacity_upper_bound, participation_bytes_hash,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use generations_v1::{C12GenerationOwnerV1, C12InspectionV1};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participation {
    pub share: ShareLink,
    pub manifest: ShareManifest,
    pub joined_unix_seconds: u64,
    /// The last commit this device agreed with the others on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreed_commit: Option<String>,
    /// The agreed commit this device has already put into the project. While
    /// it differs from `agreed_commit`, the project does not hold what the
    /// others sent, so sending from the project would take their work back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_commit: Option<String>,
    /// Paths waiting for a person because both versions were kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
}

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("sync").join(FILE)
}

/// What this device joined, if anything.
pub fn read(state_dir: &Path) -> Option<Participation> {
    let body = fs::read(path(state_dir)).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Records the join. An invitation that has run out is refused here, so an
/// old link cannot be used to join later.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn join(
    state_dir: &Path,
    share: ShareLink,
    manifest: ShareManifest,
    now_unix_seconds: u64,
) -> io::Result<Participation> {
    share
        .accept(now_unix_seconds)
        .map_err(|_| io::Error::other("この共有リンクは使えません"))?;
    if let Some(held) = read(state_dir)
        && held.share.share_id != share.share_id
    {
        // One project belongs to one share; joining a second would mix two
        // histories into the same folder.
        return Err(io::Error::other("このprojectは別の共有に参加しています"));
    }
    let participation = Participation {
        share,
        manifest,
        joined_unix_seconds: now_unix_seconds,
        agreed_commit: None,
        applied_commit: None,
        unresolved: Vec::new(),
    };
    write(state_dir, &participation)?;
    Ok(participation)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn write(state_dir: &Path, participation: &Participation) -> io::Result<()> {
    let file = path(state_dir);
    fs::create_dir_all(file.parent().expect("a parent"))?;
    let body = serde_json::to_vec_pretty(participation).map_err(io::Error::other)?;
    let temporary = file.with_extension("tmp");
    fs::write(&temporary, body)?;
    fs::rename(&temporary, file)
}

/// Stops participating on this device. The shared history stays where it is,
/// and the other participants are unaffected.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn leave(state_dir: &Path) -> io::Result<bool> {
    match fs::remove_file(path(state_dir)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn with_normal_owner<T>(
    state_dir: &Path,
    operation: impl FnOnce(&C12GenerationOwnerV1<'_>) -> std::result::Result<T, C12MutationErrorV1>,
) -> io::Result<T> {
    let lock = crate::project_mutation::ProjectMutationOwnerV1::at_store_root(
        &state_dir.join("rewind-workspace"),
    )
    .map_err(io::Error::other)?;
    let lease = lock.try_mutation().map_err(io::Error::other)?;
    if crate::apply_intent::ApplyIntentStoreV1::inspect(state_dir, &lease)
        .map_err(io::Error::other)?
        .active()
        .map_err(io::Error::other)?
        .is_some()
    {
        return Err(io::Error::other(
            "未完了の適用処理があるため参加状態を変更できません",
        ));
    }
    let owner = C12GenerationOwnerV1::open(state_dir, &lease).map_err(io::Error::other)?;
    operation(&owner).map_err(io::Error::other)
}

/// Validate and publish while holding the same project writer as apply/rewind.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn join(
    state_dir: &Path,
    share: ShareLink,
    manifest: ShareManifest,
    now_unix_seconds: u64,
) -> io::Result<Participation> {
    let participation = prepare_join(share, manifest, now_unix_seconds)?;
    with_normal_owner(state_dir, |owner| join_under_owner(owner, participation))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn prepare_join(
    share: ShareLink,
    manifest: ShareManifest,
    now_unix_seconds: u64,
) -> io::Result<Participation> {
    share
        .accept(now_unix_seconds)
        .map_err(|_| io::Error::other("この共有リンクは使えません"))?;
    Ok(Participation {
        share,
        manifest,
        joined_unix_seconds: now_unix_seconds,
        agreed_commit: None,
        applied_commit: None,
        unresolved: Vec::new(),
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn join_under_owner(
    owner: &C12GenerationOwnerV1<'_>,
    participation: Participation,
) -> std::result::Result<Participation, C12MutationErrorV1> {
    let body = serde_json::to_vec_pretty(&participation).map_err(io::Error::other)?;
    if let Some(current) = owner.current_body()? {
        let held: Participation = serde_json::from_slice(&current)
            .map_err(|_| C12MutationErrorV1::CorruptEvidence("participation schema"))?;
        if held.share.share_id != participation.share.share_id {
            return Err(io::Error::other("このprojectは別の共有に参加しています").into());
        }
    }
    owner.publish_normal(ParticipationPublicationV1::Join, Some(&body))?;
    Ok(participation)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn write(state_dir: &Path, participation: &Participation) -> io::Result<()> {
    with_normal_owner(state_dir, |owner| write_under_owner(owner, participation))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn write_under_owner(
    owner: &C12GenerationOwnerV1<'_>,
    participation: &Participation,
) -> std::result::Result<(), C12MutationErrorV1> {
    let body = serde_json::to_vec_pretty(participation).map_err(io::Error::other)?;
    owner.publish_normal(ParticipationPublicationV1::Write, Some(&body))?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn leave(state_dir: &Path) -> io::Result<bool> {
    with_normal_owner(state_dir, leave_under_owner)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn leave_under_owner(
    owner: &C12GenerationOwnerV1<'_>,
) -> std::result::Result<bool, C12MutationErrorV1> {
    let current = owner.current_body()?;
    if current.is_none()
        && matches!(
            owner.inspect()?,
            C12InspectionV1::Legacy { .. } | C12InspectionV1::Committed { pending: None, .. }
        )
    {
        return Ok(false);
    }
    owner.publish_normal(ParticipationPublicationV1::Leave, None)?;
    Ok(current.is_some())
}

#[cfg(test)]
mod tests;
