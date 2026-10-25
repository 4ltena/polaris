//! M7b-5: what this device remembers about a shared project. One small file
//! per project, next to the rest of its state. V2 binds an explicitly selected
//! credential identity; neither format contains a credential secret.
use super::remote_intent::{SyncDigestV1, SyncGitObjectIdV1};
use super::share::{CanonicalHttpsRemoteV1, ShareLink, ShareManifest};
use polaris_auth::git_credentials::{GitCredentialBindingV1, GitCredentialOwnerV1};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub const FILE: &str = "participation.json";
pub const MAX_PARTICIPATION_BYTES_V2: usize = 128 * 1024;

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
        /// Only V2 CAS publications carry this exact source association. The
        /// absent field preserves the canonical bytes of historical records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cas_source: Option<ParticipationCasSourceV2>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
    pub enum ParticipationCasSourceV2 {
        Missing,
        Present { exact_digest: SyncDigestV1 },
    }

    impl ParticipationCasSourceV2 {
        fn from_expected(expected: Option<SyncDigestV1>) -> Self {
            match expected {
                None => Self::Missing,
                Some(exact_digest) => Self::Present { exact_digest },
            }
        }
        fn exact_digest(self) -> Option<SyncDigestV1> {
            match self {
                Self::Missing => None,
                Self::Present { exact_digest } => Some(exact_digest),
            }
        }
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
                || (self.cas_source.is_some()
                    && self.publication != ParticipationPublicationV1::Write)
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
            cas_source: None,
        };
        let alloc =
            |n| allocated_bytes(n, allocation_unit).map_err(|_| C12MutationErrorV1::Overflow);
        let sum = |values: &[u64]| {
            values.iter().try_fold(0u64, |n, v| {
                n.checked_add(*v).ok_or(C12MutationErrorV1::Overflow)
            })
        };
        // Reserve the largest optional CAS association even for an older
        // caller, so the shared pre-publication bound remains sufficient.
        let cas_source_bytes = serde_json::to_vec(&ParticipationCasSourceV2::Present {
            exact_digest: SyncDigestV1::digest(b"capacity placeholder"),
        })
        .map_err(|_| C12MutationErrorV1::CorruptEvidence("CAS capacity encode"))?
        .len() as u64
            + b",\"cas_source\":".len() as u64;
        let pending_final_bytes = sum(&[
            alloc(
                (pending.canonical_bytes()?.len() as u64)
                    .checked_add(cas_source_bytes)
                    .ok_or(C12MutationErrorV1::Overflow)?,
            )?,
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
            pending: Option<(Box<C12PendingV1>, C12PendingPositionV1)>,
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
            let maximum = if matches!(name, FILE | "target-body.v1.json") {
                Some(
                    maximum
                        .unwrap_or(MAX_PARTICIPATION_BYTES_V2 as u64)
                        .min(MAX_PARTICIPATION_BYTES_V2 as u64),
                )
            } else {
                maximum
            };
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
            file.sync_all()?;
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
            #[cfg(target_pointer_width = "64")]
            let unit = raw.f_frsize;
            #[cfg(not(target_pointer_width = "64"))]
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
        pub(super) fn state_path(&self) -> &Path {
            &self.state.path
        }

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
            inspect_generation_state(&self.state, None, false)
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
        cas_source: Option<ParticipationCasSourceV2>,
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
            cas_source,
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
            cas_source: Option<ParticipationCasSourceV2>,
        ) -> Result<C12PendingV1> {
            if self.inspect()?
                != (C12InspectionV1::Committed {
                    marker: base.clone(),
                    pending: None,
                })
            {
                return Err(C12MutationErrorV1::CorruptEvidence("pending base raced"));
            }
            let pending = pending_from(base, publication, body, mutation_id, cas_source)?;
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
                if let Some(source) = cas_source
                    && sync
                        .read(FILE, None, false)?
                        .as_deref()
                        .map(SyncDigestV1::digest)
                        != source.exact_digest()
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "pending exact CAS source raced",
                    ));
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

        /// Retry only the original V2 CAS whose durable pending control still
        /// proves its source and complete target. A legacy pending record is
        /// never promoted into this authority, even for a missing source.
        pub(super) fn inspect_exact_v2_cas(
            &self,
            expected_digest: Option<SyncDigestV1>,
            bytes: &[u8],
        ) -> Result<Option<Option<Vec<u8>>>> {
            let inspected = inspect_generation_state(
                &self.state,
                Some(MAX_PARTICIPATION_BYTES_V2 as u64),
                true,
            )?;
            let (pending, position) = match inspected {
                C12InspectionV1::Legacy { .. }
                | C12InspectionV1::Committed { pending: None, .. } => return Ok(None),
                C12InspectionV1::CleanupCommitted { .. } => {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "CAS source proof retired",
                    ));
                }
                C12InspectionV1::Committed {
                    pending: Some(pair),
                    ..
                } => pair,
            };
            if pending.publication != ParticipationPublicationV1::Write
                || pending.cas_source
                    != Some(ParticipationCasSourceV2::from_expected(expected_digest))
            {
                return Err(C12MutationErrorV1::CorruptEvidence("different pending CAS"));
            }
            pending.verify_target_body(Some(bytes))?;
            let sync = self
                .sync()?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
            let current = sync.read(FILE, Some(MAX_PARTICIPATION_BYTES_V2 as u64), true)?;
            match position {
                C12PendingPositionV1::Base => {
                    if current.as_deref().map(SyncDigestV1::digest) != expected_digest {
                        return Err(C12MutationErrorV1::CorruptEvidence(
                            "pending CAS base changed",
                        ));
                    }
                    if let Some(current) = current.as_deref() {
                        let old = decode_participation_v2(current).map_err(|_| {
                            C12MutationErrorV1::CorruptEvidence("pending CAS source schema")
                        })?;
                        let next = decode_participation_v2(bytes).map_err(|_| {
                            C12MutationErrorV1::CorruptEvidence("pending CAS target schema")
                        })?;
                        if old.participation().map(|p| &p.share.share_id)
                            != next.participation().map(|p| &p.share.share_id)
                        {
                            return Err(C12MutationErrorV1::CorruptEvidence(
                                "pending CAS share mismatch",
                            ));
                        }
                    }
                }
                C12PendingPositionV1::Target | C12PendingPositionV1::Committed => {
                    if current.as_deref() != Some(bytes) {
                        return Err(C12MutationErrorV1::CorruptEvidence(
                            "pending CAS target changed",
                        ));
                    }
                }
            }
            let root =
                sync.child(GENERATION_ROOT, true)?
                    .ok_or(C12MutationErrorV1::CorruptEvidence(
                        "missing generation root",
                    ))?;
            let directory = root
                .child(PENDING_DIR, true)?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing pending"))?;
            let retained = directory.read(
                "target-body.v1.json",
                Some(MAX_PARTICIPATION_BYTES_V2 as u64),
                true,
            )?;
            if retained.as_deref() != Some(bytes)
                && !(retained.is_none()
                    && position == C12PendingPositionV1::Committed
                    && current.as_deref() == Some(bytes))
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "pending CAS exact target mismatch",
                ));
            }
            Ok(Some(current))
        }

        // Only the C10 sealed receipt entry points below may use this seam.
        // Retired CAS control never grants a new publication: the whole target
        // and committed Write marker must already exist, with empty cleanup.
        pub(super) fn inspect_c10_completed_cleanup(
            &self,
            target: &ParticipationV2,
            target_digest: SyncDigestV1,
        ) -> Result<(ParticipationGenerationV1, Vec<u8>)> {
            let bytes = target
                .canonical_bytes()
                .map_err(|_| C12MutationErrorV1::CorruptEvidence("C10 completed target schema"))?;
            if SyncDigestV1::digest(&bytes) != target_digest {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "C10 completed target digest",
                ));
            }
            let inspected = inspect_generation_state(
                &self.state,
                Some(MAX_PARTICIPATION_BYTES_V2 as u64),
                true,
            )?;
            let C12InspectionV1::CleanupCommitted { marker } = inspected else {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "C10 completed cleanup state",
                ));
            };
            if marker.generation == 0
                || marker.publication != ParticipationPublicationV1::Write
                || marker.participation_hash != Some(participation_bytes_hash(&bytes))
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "C10 completed cleanup marker",
                ));
            }
            let sync = self
                .sync()?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
            if sync
                .read(FILE, Some(MAX_PARTICIPATION_BYTES_V2 as u64), true)?
                .as_deref()
                != Some(bytes.as_slice())
                || inspect_generation_state(
                    &self.state,
                    Some(MAX_PARTICIPATION_BYTES_V2 as u64),
                    true,
                )? != (C12InspectionV1::CleanupCommitted {
                    marker: marker.clone(),
                })
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "C10 completed cleanup changed",
                ));
            }
            Ok((marker, bytes))
        }

        pub(super) fn cleanup_c10_completed_target(
            &self,
            target: &ParticipationV2,
            target_digest: SyncDigestV1,
        ) -> Result<()> {
            let (marker, bytes) = self.inspect_c10_completed_cleanup(target, target_digest)?;
            let sync = self
                .sync()?
                .ok_or(C12MutationErrorV1::CorruptEvidence("missing sync"))?;
            let root =
                sync.child(GENERATION_ROOT, true)?
                    .ok_or(C12MutationErrorV1::CorruptEvidence(
                        "missing generation root",
                    ))?;
            let directory =
                root.child(PENDING_DIR, true)?
                    .ok_or(C12MutationErrorV1::CorruptEvidence(
                        "missing cleanup directory",
                    ))?;
            if self.inspect_c10_completed_cleanup(target, target_digest)?
                != (marker.clone(), bytes.clone())
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "C10 completed cleanup changed",
                ));
            }
            // Keep this action fixed to empty-directory cleanup, even if an
            // outside writer introduces a pending control after inspection.
            self.checkpoint("cleanup-directory")?;
            root.remove_empty_child(&directory)?;
            sync.file.sync_all()?;
            if inspect_generation_state(&self.state, Some(MAX_PARTICIPATION_BYTES_V2 as u64), true)?
                != (C12InspectionV1::Committed {
                    marker,
                    pending: None,
                })
                || sync
                    .read(FILE, Some(MAX_PARTICIPATION_BYTES_V2 as u64), true)?
                    .as_deref()
                    != Some(bytes.as_slice())
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "C10 completed cleanup readback",
                ));
            }
            Ok(())
        }

        pub(super) fn resume_exact_v2_cas(
            &self,
            expected_digest: Option<SyncDigestV1>,
            bytes: &[u8],
        ) -> Result<bool> {
            if self.inspect_exact_v2_cas(expected_digest, bytes)?.is_none() {
                return Ok(false);
            }
            self.resume_pending()?;
            Ok(true)
        }

        pub(crate) fn publish_normal(
            &self,
            publication: ParticipationPublicationV1,
            body: Option<&[u8]>,
        ) -> Result<ParticipationGenerationV1> {
            self.publish_normal_checked(publication, body, None)
        }

        pub(super) fn publish_normal_cas(
            &self,
            publication: ParticipationPublicationV1,
            body: Option<&[u8]>,
            expected_digest: Option<SyncDigestV1>,
        ) -> Result<ParticipationGenerationV1> {
            self.publish_normal_checked(publication, body, Some(expected_digest))
        }

        fn publish_normal_checked(
            &self,
            publication: ParticipationPublicationV1,
            body: Option<&[u8]>,
            expected_digest: Option<Option<SyncDigestV1>>,
        ) -> Result<ParticipationGenerationV1> {
            let cas_source = expected_digest.map(ParticipationCasSourceV2::from_expected);
            let check_expected = || -> Result<()> {
                if let Some(expected) = expected_digest
                    && self.current_body()?.as_deref().map(SyncDigestV1::digest) != expected
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "participation CAS raced",
                    ));
                }
                Ok(())
            };
            check_expected()?;
            if !matches!(
                publication,
                ParticipationPublicationV1::Join
                    | ParticipationPublicationV1::Write
                    | ParticipationPublicationV1::Leave
            ) {
                return Err(C12MutationErrorV1::CorruptEvidence("normal publication"));
            }
            if matches!(self.inspect()?, C12InspectionV1::CleanupCommitted { .. }) {
                check_expected()?;
                self.resume_pending()?;
            }
            if let C12InspectionV1::Committed {
                pending: Some((pending, _)),
                ..
            } = self.inspect()?
            {
                if pending.publication != publication
                    || pending.cas_source != cas_source
                    || pending.target_body_hash != body.map(participation_bytes_hash)
                    || pending.target_body_bytes != body.map_or(0, |b| b.len() as u64)
                {
                    return Err(C12MutationErrorV1::CorruptEvidence(
                        "different pending mutation",
                    ));
                }
                check_expected()?;
                return self.resume_pending();
            }
            if matches!(self.inspect()?, C12InspectionV1::Legacy { .. }) {
                check_expected()?;
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
            check_expected()?;
            self.publish_pending(&base, publication, body, mutation_id, cas_source)?;
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
                None,
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

    // Inspection is shared by mutation owners and bounded, read-only diagnostics.
    // The latter must not acquire/create a lease or repair a pending publication.
    fn inspect_generation_state(
        state: &Directory,
        maximum: Option<u64>,
        private: bool,
    ) -> Result<C12InspectionV1> {
        let Some(sync) = state.child("sync", false)? else {
            return Ok(C12InspectionV1::Legacy {
                participation_hash: None,
            });
        };
        let current_body = sync.read(FILE, maximum, private)?;
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
            if marker.filename()? != name || records.insert(marker.generation, marker).is_some() {
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
                if sync.read(FILE, maximum, private)? != current_body {
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
            let target = directory.read("target-body.v1.json", maximum, true)?;
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
            if let Some(source) = pending.cas_source
                && (source.exact_digest().is_some() != base.participation_hash.is_some()
                    || (position == C12PendingPositionV1::Base
                        && current_body.as_deref().map(SyncDigestV1::digest)
                            != source.exact_digest()))
            {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "pending CAS source mismatch",
                ));
            }
            Some((Box::new(pending), position))
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
        if sync.read(FILE, maximum, private)? != current_body {
            return Err(C12MutationErrorV1::CorruptEvidence(
                "body raced during inspection",
            ));
        }
        Ok(C12InspectionV1::Committed { marker, pending })
    }

    pub(super) fn read_strict_participation(state: &Path) -> Result<Option<Vec<u8>>> {
        let state = match Directory::open(state, false) {
            Ok(state) => state,
            Err(C12MutationErrorV1::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Some(sync) = state.child("sync", false)? else {
            return Ok(None);
        };
        let maximum = Some(MAX_PARTICIPATION_BYTES_V2 as u64);
        let body = sync.read(FILE, maximum, true)?;
        let body_hash = body.as_deref().map(participation_bytes_hash);
        match inspect_generation_state(&state, maximum, true)? {
            C12InspectionV1::Legacy { participation_hash } if participation_hash == body_hash => (),
            C12InspectionV1::Committed {
                marker,
                pending: None,
            } if marker.participation_hash == body_hash => (),
            C12InspectionV1::Committed {
                pending: Some(_), ..
            }
            | C12InspectionV1::CleanupCommitted { .. } => {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "pending participation publication",
                ));
            }
            _ => {
                return Err(C12MutationErrorV1::CorruptEvidence(
                    "participation generation mismatch",
                ));
            }
        }
        if sync.read(FILE, maximum, true)? != body {
            return Err(C12MutationErrorV1::CorruptEvidence(
                "participation snapshot raced",
            ));
        }
        sync.verify()?;
        state.verify()?;
        Ok(body)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use generations_v1::{
    C12MutationCapacityV1, C12MutationErrorV1, C12PendingV1, ParticipationCasSourceV2,
    ParticipationGenerationV1, ParticipationPublicationV1, c12_mutation_capacity_upper_bound,
    participation_bytes_hash,
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

#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("sync").join(FILE)
}

/// V1 is retained only as the complete, validated historical field set. A V2
/// update preserves these fields and never makes a legacy locator production
/// eligible by rewriting it in place.
impl Participation {
    pub fn validate(&self) -> Result<(), ParticipationErrorV2> {
        self.share
            .validate_legacy()
            .map_err(|_| ParticipationErrorV2::Corrupt("share identity or remote"))?;
        if self.manifest.version != 1
            || self.joined_unix_seconds >= self.share.expires_unix_seconds
            || self.manifest.excluded.iter().any(|name| {
                name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.len() > 255
                    || name.contains(['/', '\\'])
                    || name.bytes().any(|byte| byte.is_ascii_control())
            })
            || [&self.agreed_commit, &self.applied_commit]
                .into_iter()
                .flatten()
                .any(|oid| SyncGitObjectIdV1::parse(oid.as_str()).is_err())
            || self.unresolved.iter().any(|name| {
                name.is_empty()
                    || name.len() > 4096
                    || name.bytes().any(|byte| byte.is_ascii_control())
                    || Path::new(name)
                        .components()
                        .any(|part| !matches!(part, std::path::Component::Normal(_)))
            })
        {
            return Err(ParticipationErrorV2::Corrupt("participation fields"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipationV2 {
    pub schema_version: u8,
    pub participation: Participation,
    pub credential_binding: GitCredentialBindingV1,
}

// The wire identity is a validated endpoint object, never a raw URL string.
// The common V1 field view remains available privately for historical callers.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareLinkWireV2 {
    remote: CanonicalHttpsRemoteV1,
    share_id: String,
    invitation_id: String,
    expires_unix_seconds: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipationFieldsWireV2 {
    share: ShareLinkWireV2,
    manifest: ShareManifest,
    joined_unix_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agreed_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unresolved: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipationWireV2 {
    schema_version: u8,
    participation: ParticipationFieldsWireV2,
    credential_binding: GitCredentialBindingV1,
}

impl Serialize for ParticipationV2 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        let value = &self.participation;
        ParticipationWireV2 {
            schema_version: self.schema_version,
            participation: ParticipationFieldsWireV2 {
                share: ShareLinkWireV2 {
                    remote: value.share.production_remote().map_err(|_| {
                        <S::Error as serde::ser::Error>::custom("production remote policy")
                    })?,
                    share_id: value.share.share_id.clone(),
                    invitation_id: value.share.invitation_id.clone(),
                    expires_unix_seconds: value.share.expires_unix_seconds,
                },
                manifest: value.manifest.clone(),
                joined_unix_seconds: value.joined_unix_seconds,
                agreed_commit: value.agreed_commit.clone(),
                applied_commit: value.applied_commit.clone(),
                unresolved: value.unresolved.clone(),
            },
            credential_binding: self.credential_binding.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ParticipationV2 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ParticipationWireV2::deserialize(deserializer)?;
        let value = wire.participation;
        let result = Self {
            schema_version: wire.schema_version,
            participation: Participation {
                share: ShareLink {
                    remote: value.share.remote.transport_url(),
                    share_id: value.share.share_id,
                    invitation_id: value.share.invitation_id,
                    expires_unix_seconds: value.share.expires_unix_seconds,
                },
                manifest: value.manifest,
                joined_unix_seconds: value.joined_unix_seconds,
                agreed_commit: value.agreed_commit,
                applied_commit: value.applied_commit,
                unresolved: value.unresolved,
            },
            credential_binding: wire.credential_binding,
        };
        result.validate().map_err(serde::de::Error::custom)?;
        Ok(result)
    }
}

impl ParticipationV2 {
    pub fn validate(&self) -> Result<(), ParticipationErrorV2> {
        if self.schema_version != 2 {
            return Err(ParticipationErrorV2::Corrupt("participation schema"));
        }
        self.participation.validate()?;
        let remote = self
            .participation
            .share
            .production_remote()
            .map_err(|_| ParticipationErrorV2::Corrupt("production remote policy"))?;
        self.credential_binding
            .validate()
            .map_err(|_| ParticipationErrorV2::Corrupt("stored credential binding"))?;
        if !self.credential_binding.matches_remote(&remote) {
            return Err(ParticipationErrorV2::Corrupt("credential remote mismatch"));
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ParticipationErrorV2> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| ParticipationErrorV2::Corrupt("participation encoding"))?;
        if bytes.len() > MAX_PARTICIPATION_BYTES_V2 {
            return Err(ParticipationErrorV2::Corrupt("participation size"));
        }
        Ok(bytes)
    }

    pub fn inner(&self) -> &Participation {
        &self.participation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParticipationReadV2 {
    Missing,
    Current(ParticipationV2),
    LegacyParticipationOnly(Participation),
    StaleCredentialBindingOnly(ParticipationV2),
}

impl ParticipationReadV2 {
    /// Historical callers may inspect the common fields only after explicit
    /// classification. This does not grant credential or mutation authority.
    pub fn participation(&self) -> Option<&Participation> {
        match self {
            Self::Missing => None,
            Self::LegacyParticipationOnly(value) => Some(value),
            Self::Current(value) | Self::StaleCredentialBindingOnly(value) => Some(value.inner()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipationSnapshotV2 {
    pub classification: ParticipationReadV2,
    /// SHA-256 of the exact bytes read, including historical whitespace.
    pub exact_digest: Option<SyncDigestV1>,
}

#[derive(Debug, thiserror::Error)]
pub enum ParticipationErrorV2 {
    #[error("invalid participation: {0}")]
    Corrupt(&'static str),
    #[error("participation changed before publication")]
    StaleExpectedDigest,
    #[error("strict participation is unavailable on this platform")]
    Unavailable,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[error(transparent)]
    Publication(#[from] C12MutationErrorV1),
}

pub(crate) fn decode_participation_v2(
    bytes: &[u8],
) -> Result<ParticipationReadV2, ParticipationErrorV2> {
    if bytes.len() > MAX_PARTICIPATION_BYTES_V2 {
        return Err(ParticipationErrorV2::Corrupt("participation size"));
    }
    let object: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| ParticipationErrorV2::Corrupt("participation JSON"))?;
    let object = object
        .as_object()
        .ok_or(ParticipationErrorV2::Corrupt("participation object"))?;
    if object.contains_key("schema_version") {
        let value: ParticipationV2 = serde_json::from_slice(bytes)
            .map_err(|_| ParticipationErrorV2::Corrupt("participation schema"))?;
        value.validate()?;
        // No Keychain or executable lookup is inferred from a stored snapshot.
        Ok(ParticipationReadV2::StaleCredentialBindingOnly(value))
    } else {
        let value: Participation = serde_json::from_slice(bytes)
            .map_err(|_| ParticipationErrorV2::Corrupt("legacy participation schema"))?;
        value.validate()?;
        Ok(ParticipationReadV2::LegacyParticipationOnly(value))
    }
}

/// Builds a canonical C12 target while retaining the validated source schema
/// and its exact credential binding. It is not a join/rebind primitive: share
/// identity and the private transport locator must remain exactly unchanged.
pub(crate) fn updated_participation_bytes_v2(
    current_bytes: &[u8],
    next: &Participation,
) -> Result<Vec<u8>, ParticipationErrorV2> {
    next.validate()?;
    let held = decode_participation_v2(current_bytes)?;
    let previous = held
        .participation()
        .ok_or(ParticipationErrorV2::Corrupt("missing update source"))?;
    if previous.share.share_id != next.share.share_id || previous.share.remote != next.share.remote
    {
        return Err(ParticipationErrorV2::Corrupt(
            "participation update identity",
        ));
    }
    let bytes = match held {
        ParticipationReadV2::Current(mut value)
        | ParticipationReadV2::StaleCredentialBindingOnly(mut value) => {
            value.participation = next.clone();
            value.canonical_bytes()?
        }
        ParticipationReadV2::LegacyParticipationOnly(_) => serde_json::to_vec(next)
            .map_err(|_| ParticipationErrorV2::Corrupt("participation encoding"))?,
        ParticipationReadV2::Missing => {
            return Err(ParticipationErrorV2::Corrupt("missing update source"));
        }
    };
    if bytes.len() > MAX_PARTICIPATION_BYTES_V2 {
        return Err(ParticipationErrorV2::Corrupt("participation size"));
    }
    Ok(bytes)
}

pub fn read_snapshot(state_dir: &Path) -> Result<ParticipationSnapshotV2, ParticipationErrorV2> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let body = generations_v1::read_strict_participation(state_dir)?;
        Ok(ParticipationSnapshotV2 {
            classification: match body.as_deref() {
                None => ParticipationReadV2::Missing,
                Some(bytes) => decode_participation_v2(bytes)?,
            },
            exact_digest: body.as_deref().map(SyncDigestV1::digest),
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = state_dir;
        Err(ParticipationErrorV2::Unavailable)
    }
}

/// Structural inspection alone is fail closed for production credentials.
pub fn read(state_dir: &Path) -> Result<ParticipationReadV2, ParticipationErrorV2> {
    Ok(read_snapshot(state_dir)?.classification)
}

/// The public promotion path invokes the credential owner's live recheck of
/// the stored Keychain binding and both executable identities. API failure,
/// missing items, ambiguity, and identity changes remain stale.
pub fn read_with_credential_owner(
    state_dir: &Path,
    owner: &GitCredentialOwnerV1,
) -> Result<ParticipationSnapshotV2, ParticipationErrorV2> {
    read_with_binding_verifier(state_dir, |binding| {
        owner.revalidate_binding(binding).is_ok()
    })
}

// This injection seam is private to this module and its focused tests. A
// caller outside it cannot promote a snapshot with an arbitrary boolean.
fn read_with_binding_verifier(
    state_dir: &Path,
    verify: impl FnOnce(&GitCredentialBindingV1) -> bool,
) -> Result<ParticipationSnapshotV2, ParticipationErrorV2> {
    let mut snapshot = read_snapshot(state_dir)?;
    snapshot.classification = match snapshot.classification {
        ParticipationReadV2::StaleCredentialBindingOnly(value) => {
            if verify(&value.credential_binding) {
                ParticipationReadV2::Current(value)
            } else {
                ParticipationReadV2::StaleCredentialBindingOnly(value)
            }
        }
        other => other,
    };
    Ok(snapshot)
}

/// C08 may inspect only its exact pending CAS without repairing or publishing.
/// Ordinary reads retain their pending blocker; arbitrary pending records are
/// never accepted by this owner-specific recovery path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn inspect_rebind_publication(
    state_dir: &Path,
    lease: &crate::project_mutation::ProjectMutationLeaseV1,
    expected_digest: Option<SyncDigestV1>,
    next: &ParticipationV2,
) -> Result<ParticipationSnapshotV2, ParticipationErrorV2> {
    let owner = C12GenerationOwnerV1::open(state_dir, lease)?;
    match owner.inspect_exact_v2_cas(expected_digest, &next.canonical_bytes()?)? {
        None => read_snapshot(state_dir),
        Some(body) => Ok(ParticipationSnapshotV2 {
            exact_digest: body.as_deref().map(SyncDigestV1::digest),
            classification: match body.as_deref() {
                None => ParticipationReadV2::Missing,
                Some(bytes) => decode_participation_v2(bytes)?,
            },
        }),
    }
}

/// The sealed C10 receipt permits inspection of its exact published target
/// after CAS source control has retired. It does not relax C08's CAS evidence.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn inspect_c10_completed_publication(
    proof: &super::push::C10CompletedParticipationRecoveryV1<'_, '_>,
) -> Result<ParticipationSnapshotV2, ParticipationErrorV2> {
    proof.verify()?;
    let owner = C12GenerationOwnerV1::open(proof.state_path(), proof.lease())?;
    let bytes = proof.target().canonical_bytes()?;
    if SyncDigestV1::digest(&bytes) != proof.target_digest() {
        return Err(ParticipationErrorV2::Corrupt("C10 completed target digest"));
    }
    let body = if matches!(owner.inspect()?, C12InspectionV1::CleanupCommitted { .. }) {
        Some(Some(
            owner
                .inspect_c10_completed_cleanup(proof.target(), proof.target_digest())?
                .1,
        ))
    } else {
        owner.inspect_exact_v2_cas(Some(proof.expected_prior()), &bytes)?
    };
    let snapshot = match body {
        None => read_snapshot(proof.state_path())?,
        Some(body) => ParticipationSnapshotV2 {
            exact_digest: body.as_deref().map(SyncDigestV1::digest),
            classification: match body.as_deref() {
                None => ParticipationReadV2::Missing,
                Some(bytes) => decode_participation_v2(bytes)?,
            },
        },
    };
    proof.verify()?;
    Ok(snapshot)
}

/// Finish only the durable C10 receipt's exact target. An already committed
/// target is idempotent; retired control permits empty cleanup alone.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn publish_c10_completed_publication(
    proof: &super::push::C10CompletedParticipationRecoveryV1<'_, '_>,
) -> Result<SyncDigestV1, ParticipationErrorV2> {
    proof.verify()?;
    let owner = C12GenerationOwnerV1::open(proof.state_path(), proof.lease())?;
    #[cfg(test)]
    if let Some(stage) = proof.test_fault() {
        owner.inject_failure(stage, libc::EIO);
    }
    let bytes = proof.target().canonical_bytes()?;
    let digest = SyncDigestV1::digest(&bytes);
    if digest != proof.target_digest() {
        return Err(ParticipationErrorV2::Corrupt("C10 completed target digest"));
    }
    match owner.inspect()? {
        C12InspectionV1::CleanupCommitted { .. } => {
            owner.inspect_c10_completed_cleanup(proof.target(), digest)?;
            proof.verify()?;
            let result = owner.cleanup_c10_completed_target(proof.target(), digest);
            proof.verify()?;
            result?;
        }
        inspected => {
            let already_published = if matches!(
                inspected,
                C12InspectionV1::Legacy { .. } | C12InspectionV1::Committed { pending: None, .. }
            ) {
                let snapshot = read_snapshot(proof.state_path())?;
                snapshot.exact_digest == Some(digest)
                    && matches!(
                        snapshot.classification,
                        ParticipationReadV2::Current(ref value)
                            | ParticipationReadV2::StaleCredentialBindingOnly(ref value)
                            if value == proof.target()
                    )
            } else {
                false
            };
            if !already_published {
                proof.verify()?;
                let result =
                    publish_v2_under_owner(&owner, Some(proof.expected_prior()), proof.target());
                proof.verify()?;
                result?;
            }
        }
    }
    let snapshot = read_snapshot(proof.state_path())?;
    if snapshot.exact_digest != Some(digest)
        || !matches!(
            snapshot.classification,
            ParticipationReadV2::Current(ref value)
                | ParticipationReadV2::StaleCredentialBindingOnly(ref value)
                if value == proof.target()
        )
    {
        return Err(ParticipationErrorV2::Corrupt(
            "C10 completed publication readback",
        ));
    }
    proof.verify()?;
    Ok(digest)
}

/// Publication is lease-bound and uses the existing C12 receipt chain. C08
/// and C10 owners must establish their own durable EffectStarted/BeforeWrite
/// before calling this shared primitive.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn publish_v2(
    state_dir: &Path,
    lease: &crate::project_mutation::ProjectMutationLeaseV1,
    expected_digest: Option<SyncDigestV1>,
    next: &ParticipationV2,
) -> Result<SyncDigestV1, ParticipationErrorV2> {
    let owner = C12GenerationOwnerV1::open(state_dir, lease)?;
    publish_v2_under_owner(&owner, expected_digest, next)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_v2_under_owner(
    owner: &C12GenerationOwnerV1<'_>,
    expected_digest: Option<SyncDigestV1>,
    next: &ParticipationV2,
) -> Result<SyncDigestV1, ParticipationErrorV2> {
    let bytes = next.canonical_bytes()?;
    if !owner.resume_exact_v2_cas(expected_digest, &bytes)? {
        let snapshot = read_snapshot(owner.state_path())?;
        if snapshot.exact_digest != expected_digest {
            return Err(ParticipationErrorV2::StaleExpectedDigest);
        }
        if let Some(old) = snapshot.classification.participation()
            && old.share.share_id != next.participation.share.share_id
        {
            return Err(ParticipationErrorV2::Corrupt("different share identity"));
        }
        owner.publish_normal_cas(
            ParticipationPublicationV1::Write,
            Some(&bytes),
            expected_digest,
        )?;
    }
    let actual = read_snapshot(owner.state_path())?;
    let digest = SyncDigestV1::digest(&bytes);
    if actual.exact_digest != Some(digest)
        || actual.classification.participation() != Some(next.inner())
    {
        return Err(ParticipationErrorV2::Corrupt(
            "participation publication readback",
        ));
    }
    Ok(digest)
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
        .map_err(|_| io::Error::other("This sharing link cannot be used"))?;
    if let Some(held) = read(state_dir).map_err(io::Error::other)?.participation()
        && held.share.share_id != share.share_id
    {
        // One project belongs to one share; joining a second would mix two
        // histories into the same folder.
        return Err(io::Error::other(
            "This project has joined a different share",
        ));
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
    crate::apply_intent::ApplyIntentStoreV1::inspect(state_dir, &lease)
        .and_then(|store| store.require_idle())
        .map_err(io::Error::other)?;
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
        .map_err(|_| io::Error::other("This sharing link cannot be used"))?;
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
    participation
        .validate()
        .map_err(|_| C12MutationErrorV1::CorruptEvidence("participation fields"))?;
    let body = serde_json::to_vec_pretty(&participation).map_err(io::Error::other)?;
    if body.len() > MAX_PARTICIPATION_BYTES_V2 {
        return Err(C12MutationErrorV1::CorruptEvidence("participation size"));
    }
    if let Some(current) = owner.current_body()? {
        let classification = decode_participation_v2(&current)
            .map_err(|_| C12MutationErrorV1::CorruptEvidence("participation schema"))?;
        let ParticipationReadV2::LegacyParticipationOnly(held) = classification else {
            return Err(C12MutationErrorV1::CorruptEvidence(
                "V2 requires bound join",
            ));
        };
        if held.share.share_id != participation.share.share_id {
            return Err(io::Error::other("This project has joined a different share").into());
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
    participation
        .validate()
        .map_err(|_| C12MutationErrorV1::CorruptEvidence("participation fields"))?;
    let body = match owner.current_body()? {
        Some(current) => updated_participation_bytes_v2(&current, participation)
            .map_err(|_| C12MutationErrorV1::CorruptEvidence("participation update"))?,
        None => serde_json::to_vec_pretty(participation).map_err(io::Error::other)?,
    };
    if body.len() > MAX_PARTICIPATION_BYTES_V2 {
        return Err(C12MutationErrorV1::CorruptEvidence("participation size"));
    }
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
    if let Some(bytes) = current.as_deref() {
        decode_participation_v2(bytes)
            .map_err(|_| C12MutationErrorV1::CorruptEvidence("participation schema"))?;
    }
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
