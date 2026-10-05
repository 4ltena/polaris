//! Durable owner for sync operations that may mutate a remote ref.
//!
//! The owner persists `BeforeSend` with file and directory fsync before it
//! calls the transport. Once dispatch can have happened, every failure is an
//! unresolved outcome. The owner never retries a send automatically.

use super::remote_intent::{
    CurrentSyncRemoteStateV1, SyncRemoteIntentRecordV1, SyncRemoteIntentRequestV1,
    SyncRemoteIntentStateV1, SyncRemoteOperationIdV1, SyncRemoteReceiptV1,
    SyncRemoteUnknownReasonV1,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

const STORE_DIRECTORY: &str = "remote-intents-v1";
const MAX_RECORD_BYTES: u64 = 128 * 1024;

pub trait SyncRemoteTransportV1 {
    type Error;

    /// Dispatches exactly the request supplied by the durable owner. A
    /// transport error never proves that the remote did not accept it.
    fn send(
        &mut self,
        request: &SyncRemoteIntentRequestV1,
    ) -> Result<SyncRemoteReceiptV1, Self::Error>;

    /// Reads the exact remote ref named by the request without updating it.
    fn observe_exact_ref(
        &mut self,
        request: &SyncRemoteIntentRequestV1,
    ) -> Result<Option<super::remote_intent::SyncGitObjectIdV1>, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRemotePushOutcomeV1 {
    Completed(Box<SyncRemoteReceiptV1>),
    OutcomeUnknown {
        reason: SyncRemoteUnknownReasonV1,
        /// False means the durable `BeforeSend` record is still the recovery
        /// evidence because the later state could not be published.
        unknown_state_persisted: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRemoteRecoveryV1 {
    Completed(Box<SyncRemoteReceiptV1>),
    Unresolved(Box<SyncRemoteIntentRecordV1>),
}

#[derive(Debug, thiserror::Error)]
pub enum SyncRemoteOwnerErrorV1 {
    #[error("sync remote intent store is invalid: {0}")]
    InvalidStore(PathBuf),
    #[error("sync remote intent record is invalid")]
    InvalidRecord,
    #[error("sync remote intent owner is busy")]
    Busy,
    #[error("sync remote intent I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("operation id already has a durable sync remote intent")]
    ExistingIntent,
    #[error("operation id conflicts with another durable sync remote intent")]
    ConflictingIntent,
    #[error("stale sync project identity")]
    StaleProject,
    #[error("stale sync share identity")]
    StaleShare,
    #[error("stale canonical sync remote")]
    StaleRemote,
    #[error("stale exact sync remote ref")]
    StaleRemoteRef,
    #[error("stale local sync commit")]
    StaleLocalCommit,
    #[error("stale expected remote object id")]
    StaleExpectedRemoteOid,
    #[error("stale authenticated account binding")]
    StaleAccountBinding,
    #[error("stale sync permission scope")]
    StalePermission,
}

pub struct SyncRemoteIntentOwnerV1 {
    store: FsRemoteIntentStoreV1,
}

impl SyncRemoteIntentOwnerV1 {
    /// Opens the remote intent store below this project's already-resolved
    /// Polaris state directory. No credential is read or inferred here.
    pub fn open(state_dir: &Path) -> Result<Self, SyncRemoteOwnerErrorV1> {
        Ok(Self {
            store: FsRemoteIntentStoreV1::open(&state_dir.join("sync").join(STORE_DIRECTORY))?,
        })
    }

    /// Sends at most once. Any existing record, including an unresolved one,
    /// refuses dispatch and must be inspected or reconciled instead.
    pub fn send_once<T: SyncRemoteTransportV1>(
        &mut self,
        request: SyncRemoteIntentRequestV1,
        current: &CurrentSyncRemoteStateV1,
        transport: &mut T,
    ) -> Result<SyncRemotePushOutcomeV1, SyncRemoteOwnerErrorV1> {
        validate_current(&request, current)?;
        let _lease = self.store.try_lock()?;
        match self.store.read(&request.operation_id)? {
            Some(record) if record.request == request => {
                return Err(SyncRemoteOwnerErrorV1::ExistingIntent);
            }
            Some(_) => return Err(SyncRemoteOwnerErrorV1::ConflictingIntent),
            None => {}
        }

        let mut record = SyncRemoteIntentRecordV1::before_send(request);
        // This file sync and parent-directory sync complete before dispatch.
        self.store.write(&record)?;

        let receipt = match transport.send(&record.request) {
            Ok(receipt) if receipt.proves(&record.request) => receipt,
            Ok(_) => {
                return self
                    .persist_unknown(record, SyncRemoteUnknownReasonV1::InvalidRemoteReceipt);
            }
            Err(_) => {
                return self.persist_unknown(record, SyncRemoteUnknownReasonV1::TransportUncertain);
            }
        };

        record.state = SyncRemoteIntentStateV1::Completed {
            receipt: Box::new(receipt.clone()),
        };
        if self.store.write(&record).is_ok() {
            return Ok(SyncRemotePushOutcomeV1::Completed(Box::new(receipt)));
        }

        // Dispatch was acknowledged but the local receipt was not durable.
        // A second best-effort write records the uncertainty; if storage is
        // unavailable, the durable BeforeSend record still blocks a retry.
        record.state = SyncRemoteIntentStateV1::OutcomeUnknown {
            reason: SyncRemoteUnknownReasonV1::ReceiptPersistenceFailed,
        };
        let persisted = self.store.write(&record).is_ok();
        Ok(SyncRemotePushOutcomeV1::OutcomeUnknown {
            reason: SyncRemoteUnknownReasonV1::ReceiptPersistenceFailed,
            unknown_state_persisted: persisted,
        })
    }

    /// Loads an operation after restart. A durable BeforeSend record means
    /// dispatch may have happened, so it is converted to OutcomeUnknown and
    /// is never offered as an automatic retry.
    pub fn recover(
        &mut self,
        operation_id: &SyncRemoteOperationIdV1,
        current: &CurrentSyncRemoteStateV1,
    ) -> Result<Option<SyncRemoteRecoveryV1>, SyncRemoteOwnerErrorV1> {
        let _lease = self.store.try_lock()?;
        let Some(mut record) = self.store.read(operation_id)? else {
            return Ok(None);
        };
        if let SyncRemoteIntentStateV1::Completed { receipt } = &record.state {
            return Ok(Some(SyncRemoteRecoveryV1::Completed(receipt.clone())));
        }
        validate_current(&record.request, current)?;
        if matches!(record.state, SyncRemoteIntentStateV1::BeforeSend) {
            record.state = SyncRemoteIntentStateV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::RestartedAfterBeforeSend,
            };
            self.store.write(&record)?;
        }
        Ok(Some(SyncRemoteRecoveryV1::Unresolved(Box::new(record))))
    }

    /// Reconciles only from proof that the exact requested ref currently
    /// equals the exact local commit. Missing, different or unreadable refs
    /// remain unresolved and never trigger another send.
    pub fn reconcile<T: SyncRemoteTransportV1>(
        &mut self,
        operation_id: &SyncRemoteOperationIdV1,
        current: &CurrentSyncRemoteStateV1,
        transport: &mut T,
    ) -> Result<Option<SyncRemoteRecoveryV1>, SyncRemoteOwnerErrorV1> {
        let _lease = self.store.try_lock()?;
        let Some(mut record) = self.store.read(operation_id)? else {
            return Ok(None);
        };
        if let SyncRemoteIntentStateV1::Completed { receipt } = &record.state {
            return Ok(Some(SyncRemoteRecoveryV1::Completed(receipt.clone())));
        }
        validate_current(&record.request, current)?;

        match transport.observe_exact_ref(&record.request) {
            Ok(Some(observed)) if observed == record.request.local_commit => {
                let receipt = SyncRemoteReceiptV1::reconciled_exact_ref(&record.request);
                record.state = SyncRemoteIntentStateV1::Completed {
                    receipt: Box::new(receipt.clone()),
                };
                if self.store.write(&record).is_ok() {
                    return Ok(Some(SyncRemoteRecoveryV1::Completed(Box::new(receipt))));
                }
                record.state = SyncRemoteIntentStateV1::OutcomeUnknown {
                    reason: SyncRemoteUnknownReasonV1::ReceiptPersistenceFailed,
                };
                let _ = self.store.write(&record);
                Ok(Some(SyncRemoteRecoveryV1::Unresolved(Box::new(record))))
            }
            Ok(_) => {
                record.state = SyncRemoteIntentStateV1::OutcomeUnknown {
                    reason: SyncRemoteUnknownReasonV1::RemoteRefDidNotMatch,
                };
                self.store.write(&record)?;
                Ok(Some(SyncRemoteRecoveryV1::Unresolved(Box::new(record))))
            }
            Err(_) => {
                record.state = SyncRemoteIntentStateV1::OutcomeUnknown {
                    reason: SyncRemoteUnknownReasonV1::ReconciliationUnavailable,
                };
                self.store.write(&record)?;
                Ok(Some(SyncRemoteRecoveryV1::Unresolved(Box::new(record))))
            }
        }
    }

    fn persist_unknown(
        &mut self,
        mut record: SyncRemoteIntentRecordV1,
        reason: SyncRemoteUnknownReasonV1,
    ) -> Result<SyncRemotePushOutcomeV1, SyncRemoteOwnerErrorV1> {
        record.state = SyncRemoteIntentStateV1::OutcomeUnknown { reason };
        let persisted = self.store.write(&record).is_ok();
        Ok(SyncRemotePushOutcomeV1::OutcomeUnknown {
            reason,
            unknown_state_persisted: persisted,
        })
    }

    #[cfg(test)]
    fn fail_next_completed_write(&mut self) {
        self.store.fail_next_completed_write = true;
    }

    #[cfg(test)]
    fn fail_write_after(&mut self, successful_writes: usize) {
        self.store.fail_on_write_call = Some(self.store.write_calls + successful_writes + 1);
    }
}

fn validate_current(
    request: &SyncRemoteIntentRequestV1,
    current: &CurrentSyncRemoteStateV1,
) -> Result<(), SyncRemoteOwnerErrorV1> {
    if request.project != current.project {
        return Err(SyncRemoteOwnerErrorV1::StaleProject);
    }
    if request.share_id != current.share_id {
        return Err(SyncRemoteOwnerErrorV1::StaleShare);
    }
    if request.canonical_remote != current.canonical_remote {
        return Err(SyncRemoteOwnerErrorV1::StaleRemote);
    }
    if request.remote_ref != current.remote_ref {
        return Err(SyncRemoteOwnerErrorV1::StaleRemoteRef);
    }
    if request.local_commit != current.local_commit {
        return Err(SyncRemoteOwnerErrorV1::StaleLocalCommit);
    }
    if request.expected_remote_oid != current.expected_remote_oid {
        return Err(SyncRemoteOwnerErrorV1::StaleExpectedRemoteOid);
    }
    if request.account_binding != current.account_binding {
        return Err(SyncRemoteOwnerErrorV1::StaleAccountBinding);
    }
    if request.permission_hash != current.permission_hash {
        return Err(SyncRemoteOwnerErrorV1::StalePermission);
    }
    Ok(())
}

struct FsRemoteIntentStoreV1 {
    root: PathBuf,
    directory: File,
    #[cfg(test)]
    fail_next_completed_write: bool,
    #[cfg(test)]
    fail_on_write_call: Option<usize>,
    #[cfg(test)]
    write_calls: usize,
}

impl FsRemoteIntentStoreV1 {
    fn open(root: &Path) -> Result<Self, SyncRemoteOwnerErrorV1> {
        let parent = root
            .parent()
            .ok_or_else(|| SyncRemoteOwnerErrorV1::InvalidStore(root.to_path_buf()))?;
        fs::create_dir_all(parent)?;
        if let Err(error) = fs::create_dir(root)
            && error.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(error.into());
        }
        let before_open = fs::symlink_metadata(root)?;
        if !before_open.file_type().is_dir() || before_open.uid() != unsafe { libc::geteuid() } {
            return Err(SyncRemoteOwnerErrorV1::InvalidStore(root.to_path_buf()));
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .map_err(|error| map_store_open_error(root, error))?;
        validate_directory(&directory, root)?;
        let canonical = fs::canonicalize(root)?;
        let canonical_meta = fs::metadata(&canonical)?;
        let held_meta = directory.metadata()?;
        if canonical_meta.dev() != held_meta.dev() || canonical_meta.ino() != held_meta.ino() {
            return Err(SyncRemoteOwnerErrorV1::InvalidStore(root.to_path_buf()));
        }
        let store = Self {
            root: canonical,
            directory,
            #[cfg(test)]
            fail_next_completed_write: false,
            #[cfg(test)]
            fail_on_write_call: None,
            #[cfg(test)]
            write_calls: 0,
        };
        let lock = store.open_named("owner.lock", libc::O_RDWR | libc::O_CREAT, 0o600)?;
        validate_record_file(&lock, &store.root.join("owner.lock"))?;
        store.directory.sync_all()?;
        Ok(store)
    }

    fn try_lock(&self) -> Result<RemoteIntentLeaseV1, SyncRemoteOwnerErrorV1> {
        let file = self.open_named("owner.lock", libc::O_RDWR, 0o600)?;
        validate_record_file(&file, &self.root.join("owner.lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                Err(SyncRemoteOwnerErrorV1::Busy)
            } else {
                Err(error.into())
            };
        }
        Ok(RemoteIntentLeaseV1(file))
    }

    fn read(
        &self,
        operation_id: &SyncRemoteOperationIdV1,
    ) -> Result<Option<SyncRemoteIntentRecordV1>, SyncRemoteOwnerErrorV1> {
        let name = record_name(operation_id);
        let file = match self.open_named(&name, libc::O_RDONLY, 0) {
            Ok(file) => file,
            Err(SyncRemoteOwnerErrorV1::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        validate_record_file(&file, &self.root.join(&name))?;
        let mut body = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut body)?;
        if body.len() as u64 > MAX_RECORD_BYTES {
            return Err(SyncRemoteOwnerErrorV1::InvalidRecord);
        }
        let record: SyncRemoteIntentRecordV1 =
            serde_json::from_slice(&body).map_err(|_| SyncRemoteOwnerErrorV1::InvalidRecord)?;
        if record.version != SyncRemoteIntentRecordV1::VERSION
            || record.request.operation_id != *operation_id
        {
            return Err(SyncRemoteOwnerErrorV1::InvalidRecord);
        }
        validate_record_invariants(&record)?;
        Ok(Some(record))
    }

    fn write(&mut self, record: &SyncRemoteIntentRecordV1) -> Result<(), SyncRemoteOwnerErrorV1> {
        validate_record_invariants(record)?;
        #[cfg(test)]
        {
            self.write_calls += 1;
            if self.fail_on_write_call == Some(self.write_calls) {
                self.fail_on_write_call = None;
                return Err(io::Error::other("injected intent publication failure").into());
            }
        }
        #[cfg(test)]
        if self.fail_next_completed_write
            && matches!(record.state, SyncRemoteIntentStateV1::Completed { .. })
        {
            self.fail_next_completed_write = false;
            return Err(io::Error::other("injected receipt publication failure").into());
        }

        self.write_serialized(record)
    }

    fn write_serialized(
        &mut self,
        record: &SyncRemoteIntentRecordV1,
    ) -> Result<(), SyncRemoteOwnerErrorV1> {
        let mut body =
            serde_json::to_vec_pretty(record).map_err(|_| SyncRemoteOwnerErrorV1::InvalidRecord)?;
        body.push(b'\n');
        if body.len() as u64 > MAX_RECORD_BYTES {
            return Err(SyncRemoteOwnerErrorV1::InvalidRecord);
        }

        let name = record_name(&record.request.operation_id);
        let temporary = temporary_name(&name)?;
        let mut file = self.open_named(
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let publish = (|| -> Result<(), SyncRemoteOwnerErrorV1> {
            validate_record_file(&file, &self.root.join(&temporary))?;
            file.write_all(&body)?;
            file.sync_all()?;
            rename_at(&self.directory, &temporary, &name)?;
            self.directory.sync_all()?;
            Ok(())
        })();
        if publish.is_err() {
            let _ = unlink_at(&self.directory, &temporary);
        }
        publish
    }

    fn open_named(
        &self,
        name: &str,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<File, SyncRemoteOwnerErrorV1> {
        let name = c_name(name)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                libc::c_uint::from(mode),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn validate_record_invariants(
    record: &SyncRemoteIntentRecordV1,
) -> Result<(), SyncRemoteOwnerErrorV1> {
    if record.version != SyncRemoteIntentRecordV1::VERSION {
        return Err(SyncRemoteOwnerErrorV1::InvalidRecord);
    }
    if let SyncRemoteIntentStateV1::Completed { receipt } = &record.state
        && !receipt.proves(&record.request)
    {
        return Err(SyncRemoteOwnerErrorV1::InvalidRecord);
    }
    Ok(())
}

fn record_name(operation_id: &SyncRemoteOperationIdV1) -> String {
    let digest: [u8; 32] = Sha256::digest(operation_id.as_str().as_bytes()).into();
    let mut name = String::with_capacity(64 + ".json".len());
    for byte in digest {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name.push_str(".json");
    name
}

fn temporary_name(target: &str) -> Result<String, SyncRemoteOwnerErrorV1> {
    let mut random = [0_u8; 12];
    getrandom::fill(&mut random)
        .map_err(|error| io::Error::other(format!("temporary name entropy: {error}")))?;
    let mut name = format!(".{target}.");
    for byte in random {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name.push_str(".tmp");
    Ok(name)
}

fn c_name(name: &str) -> Result<CString, SyncRemoteOwnerErrorV1> {
    CString::new(name).map_err(|_| SyncRemoteOwnerErrorV1::InvalidRecord)
}

fn rename_at(directory: &File, from: &str, to: &str) -> io::Result<()> {
    let from = CString::new(from).expect("generated file name has no NUL");
    let to = CString::new(to).expect("generated file name has no NUL");
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_at(directory: &File, name: &str) -> io::Result<()> {
    let name = CString::new(name).expect("generated file name has no NUL");
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn map_store_open_error(root: &Path, error: io::Error) -> SyncRemoteOwnerErrorV1 {
    if matches!(error.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENOTDIR) {
        SyncRemoteOwnerErrorV1::InvalidStore(root.to_path_buf())
    } else {
        SyncRemoteOwnerErrorV1::Io(error)
    }
}

fn validate_directory(file: &File, path: &Path) -> Result<(), SyncRemoteOwnerErrorV1> {
    let metadata = file.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7077 != 0
        || metadata.mode() & 0o700 != 0o700
    {
        return Err(SyncRemoteOwnerErrorV1::InvalidStore(path.to_path_buf()));
    }
    Ok(())
}

fn validate_record_file(file: &File, path: &Path) -> Result<(), SyncRemoteOwnerErrorV1> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7177 != 0
    {
        return Err(SyncRemoteOwnerErrorV1::InvalidStore(path.to_path_buf()));
    }
    Ok(())
}

struct RemoteIntentLeaseV1(File);

impl Drop for RemoteIntentLeaseV1 {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::remote_intent::{
        CanonicalSyncRemoteV1, ExpectedSyncRemoteOidV1, SyncAccountBindingV1,
        SyncAccountProviderV1, SyncAccountRegistrationIdV1, SyncDigestV1, SyncGitObjectIdV1,
        SyncRemoteIdempotencyKeyV1, SyncRemoteReceiptIdV1, SyncRemoteRefV1, SyncShareIdV1,
    };

    #[derive(Default)]
    struct SyntheticTransport {
        sends: usize,
        observations: usize,
        send_result: Option<Result<SyncRemoteReceiptV1, ()>>,
        observed: Option<Result<Option<SyncGitObjectIdV1>, ()>>,
    }

    impl SyncRemoteTransportV1 for SyntheticTransport {
        type Error = ();

        fn send(
            &mut self,
            request: &SyncRemoteIntentRequestV1,
        ) -> Result<SyncRemoteReceiptV1, Self::Error> {
            self.sends += 1;
            self.send_result.take().unwrap_or_else(|| {
                Ok(SyncRemoteReceiptV1::transport_acknowledged(
                    request,
                    SyncRemoteReceiptIdV1::parse("receipt-1").unwrap(),
                    request.local_commit.clone(),
                ))
            })
        }

        fn observe_exact_ref(
            &mut self,
            _request: &SyncRemoteIntentRequestV1,
        ) -> Result<Option<SyncGitObjectIdV1>, Self::Error> {
            self.observations += 1;
            self.observed.take().unwrap_or(Ok(None))
        }
    }

    fn oid(character: char) -> SyncGitObjectIdV1 {
        SyncGitObjectIdV1::parse(character.to_string().repeat(40)).unwrap()
    }

    fn binding(registration: &str) -> SyncAccountBindingV1 {
        SyncAccountBindingV1::new(
            SyncAccountProviderV1::parse("openai").unwrap(),
            SyncAccountRegistrationIdV1::parse(registration).unwrap(),
            SyncDigestV1::digest(format!("subject:{registration}").as_bytes()),
        )
    }

    fn fixture(operation: &str) -> (SyncRemoteIntentRequestV1, CurrentSyncRemoteStateV1) {
        let account_binding = binding("account-1");
        let request = SyncRemoteIntentRequestV1 {
            operation_id: SyncRemoteOperationIdV1::parse(operation).unwrap(),
            idempotency_key: SyncRemoteIdempotencyKeyV1::parse(format!("idem-{operation}"))
                .unwrap(),
            project: SyncDigestV1::digest(b"project"),
            share_id: SyncShareIdV1::parse("share-1").unwrap(),
            canonical_remote: CanonicalSyncRemoteV1::from_canonical(
                "https://example.invalid/share.git",
            )
            .unwrap(),
            remote_ref: SyncRemoteRefV1::parse("refs/heads/polaris").unwrap(),
            local_commit: oid('a'),
            expected_remote_oid: ExpectedSyncRemoteOidV1::Present { oid: oid('b') },
            account_binding,
            permission_hash: SyncDigestV1::digest(b"sync-push-permission"),
        };
        let current = CurrentSyncRemoteStateV1::new(
            request.project,
            request.share_id.clone(),
            request.canonical_remote.clone(),
            request.remote_ref.clone(),
            request.local_commit.clone(),
            request.expected_remote_oid.clone(),
            request.account_binding.clone(),
            request.permission_hash,
        );
        (request, current)
    }

    #[test]
    fn success_then_local_receipt_write_failure_is_durable_unknown_and_never_retries() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-receipt-failure");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        owner.fail_next_completed_write();
        let mut transport = SyntheticTransport::default();

        let outcome = owner
            .send_once(request.clone(), &current, &mut transport)
            .unwrap();
        assert_eq!(
            outcome,
            SyncRemotePushOutcomeV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::ReceiptPersistenceFailed,
                unknown_state_persisted: true,
            }
        );
        assert_eq!(transport.sends, 1);

        let retry = owner.send_once(request, &current, &mut transport);
        assert!(matches!(retry, Err(SyncRemoteOwnerErrorV1::ExistingIntent)));
        assert_eq!(transport.sends, 1);

        drop(owner);
        let mut restarted = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let recovered = restarted.recover(&operation, &current).unwrap().unwrap();
        let SyncRemoteRecoveryV1::Unresolved(record) = recovered else {
            panic!("receipt publication failure must stay unresolved");
        };
        assert!(matches!(
            record.state,
            SyncRemoteIntentStateV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::ReceiptPersistenceFailed
            }
        ));
    }

    #[test]
    fn before_send_is_unknown_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-restart");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        owner
            .store
            .write(&SyncRemoteIntentRecordV1::before_send(request))
            .unwrap();
        drop(owner);

        let mut restarted = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let recovered = restarted.recover(&operation, &current).unwrap().unwrap();
        let SyncRemoteRecoveryV1::Unresolved(record) = recovered else {
            panic!("BeforeSend after restart must stay unresolved");
        };
        assert!(matches!(
            record.state,
            SyncRemoteIntentStateV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::RestartedAfterBeforeSend
            }
        ));
    }

    #[test]
    fn exact_remote_ref_reconciles_without_resending() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-reconcile");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let mut transport = SyntheticTransport {
            send_result: Some(Err(())),
            ..Default::default()
        };
        let outcome = owner.send_once(request, &current, &mut transport).unwrap();
        assert!(matches!(
            outcome,
            SyncRemotePushOutcomeV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::TransportUncertain,
                ..
            }
        ));

        transport.observed = Some(Ok(Some(current.local_commit.clone())));
        let recovered = owner
            .reconcile(&operation, &current, &mut transport)
            .unwrap()
            .unwrap();
        let SyncRemoteRecoveryV1::Completed(receipt) = recovered else {
            panic!("the exact ref must reconcile as completed");
        };
        assert!(matches!(
            receipt.source,
            super::super::remote_intent::SyncRemoteReceiptSourceV1::ReconciledExactRef
        ));
        assert_eq!(transport.sends, 1);
        assert_eq!(transport.observations, 1);
    }

    #[test]
    fn completed_receipt_recovers_after_normal_account_and_commit_drift() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-completed-drift");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let mut transport = SyntheticTransport::default();
        assert!(matches!(
            owner.send_once(request, &current, &mut transport).unwrap(),
            SyncRemotePushOutcomeV1::Completed(_)
        ));
        drop(owner);

        let mut drifted = current.clone();
        drifted.account_binding = binding("account-2");
        drifted.local_commit = oid('c');
        let mut restarted = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        assert!(matches!(
            restarted.recover(&operation, &drifted).unwrap().unwrap(),
            SyncRemoteRecoveryV1::Completed(_)
        ));
        assert!(matches!(
            restarted
                .reconcile(&operation, &drifted, &mut transport)
                .unwrap()
                .unwrap(),
            SyncRemoteRecoveryV1::Completed(_)
        ));
        assert_eq!(transport.observations, 0);
    }

    #[test]
    fn a_completed_record_with_a_mismatched_receipt_is_rejected() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-forged-receipt");
        let operation = request.operation_id.clone();
        let mut receipt = SyncRemoteReceiptV1::transport_acknowledged(
            &request,
            SyncRemoteReceiptIdV1::parse("receipt-forged").unwrap(),
            request.local_commit.clone(),
        );
        receipt.idempotency_key = SyncRemoteIdempotencyKeyV1::parse("another-key").unwrap();
        let record = SyncRemoteIntentRecordV1 {
            version: SyncRemoteIntentRecordV1::VERSION,
            request,
            state: SyncRemoteIntentStateV1::Completed {
                receipt: Box::new(receipt),
            },
        };
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        owner.store.write_serialized(&record).unwrap();
        assert!(matches!(
            owner.recover(&operation, &current),
            Err(SyncRemoteOwnerErrorV1::InvalidRecord)
        ));
    }

    #[test]
    fn reconciled_receipt_write_failure_remains_unknown() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-reconcile-receipt-failure");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let mut transport = SyntheticTransport {
            send_result: Some(Err(())),
            observed: Some(Ok(Some(current.local_commit.clone()))),
            ..Default::default()
        };
        owner.send_once(request, &current, &mut transport).unwrap();
        owner.fail_next_completed_write();

        let recovered = owner
            .reconcile(&operation, &current, &mut transport)
            .unwrap()
            .unwrap();
        let SyncRemoteRecoveryV1::Unresolved(record) = recovered else {
            panic!("an unpublished reconciliation receipt must stay unresolved");
        };
        assert!(matches!(
            record.state,
            SyncRemoteIntentStateV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::ReceiptPersistenceFailed
            }
        ));
        assert_eq!(transport.sends, 1);
    }

    #[test]
    fn unknown_transition_write_failure_keeps_before_send_as_restart_barrier() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-unknown-write-failure");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        owner.fail_write_after(1);
        let mut transport = SyntheticTransport {
            send_result: Some(Err(())),
            ..Default::default()
        };
        assert_eq!(
            owner.send_once(request, &current, &mut transport).unwrap(),
            SyncRemotePushOutcomeV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::TransportUncertain,
                unknown_state_persisted: false,
            }
        );
        assert_eq!(transport.sends, 1);
        drop(owner);

        let mut restarted = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let recovered = restarted.recover(&operation, &current).unwrap().unwrap();
        let SyncRemoteRecoveryV1::Unresolved(record) = recovered else {
            panic!("a durable BeforeSend record must block retry after restart");
        };
        assert!(matches!(
            record.state,
            SyncRemoteIntentStateV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::RestartedAfterBeforeSend
            }
        ));
    }

    #[test]
    fn the_owner_lock_refuses_concurrent_send_and_reconcile() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-owner-busy");
        let operation = request.operation_id.clone();
        let first = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let mut second = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let _lease = first.store.try_lock().unwrap();
        let mut transport = SyntheticTransport::default();

        assert!(matches!(
            second.send_once(request, &current, &mut transport),
            Err(SyncRemoteOwnerErrorV1::Busy)
        ));
        assert!(matches!(
            second.reconcile(&operation, &current, &mut transport),
            Err(SyncRemoteOwnerErrorV1::Busy)
        ));
        assert_eq!(transport.sends, 0);
        assert_eq!(transport.observations, 0);
    }

    #[test]
    fn stale_account_ref_and_commit_are_refused_before_transport_or_intent() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-stale");
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let mut transport = SyntheticTransport::default();

        let mut stale_account = current.clone();
        stale_account.account_binding = binding("account-2");
        assert!(matches!(
            owner.send_once(request.clone(), &stale_account, &mut transport),
            Err(SyncRemoteOwnerErrorV1::StaleAccountBinding)
        ));

        let mut stale_ref = current.clone();
        stale_ref.remote_ref = SyncRemoteRefV1::parse("refs/heads/other").unwrap();
        assert!(matches!(
            owner.send_once(request.clone(), &stale_ref, &mut transport),
            Err(SyncRemoteOwnerErrorV1::StaleRemoteRef)
        ));

        let mut stale_commit = current.clone();
        stale_commit.local_commit = oid('c');
        assert!(matches!(
            owner.send_once(request.clone(), &stale_commit, &mut transport),
            Err(SyncRemoteOwnerErrorV1::StaleLocalCommit)
        ));

        assert_eq!(transport.sends, 0);
        assert!(owner.store.read(&request.operation_id).unwrap().is_none());
    }

    #[test]
    fn a_different_or_missing_remote_ref_stays_unresolved() {
        let temporary = tempfile::tempdir().unwrap();
        let (request, current) = fixture("op-mismatch");
        let operation = request.operation_id.clone();
        let mut owner = SyncRemoteIntentOwnerV1::open(temporary.path()).unwrap();
        let mut transport = SyntheticTransport {
            send_result: Some(Err(())),
            observed: Some(Ok(Some(oid('d')))),
            ..Default::default()
        };
        owner.send_once(request, &current, &mut transport).unwrap();
        let recovered = owner
            .reconcile(&operation, &current, &mut transport)
            .unwrap()
            .unwrap();
        let SyncRemoteRecoveryV1::Unresolved(record) = recovered else {
            panic!("a different remote ref must stay unresolved");
        };
        assert!(matches!(
            record.state,
            SyncRemoteIntentStateV1::OutcomeUnknown {
                reason: SyncRemoteUnknownReasonV1::RemoteRefDidNotMatch
            }
        ));
        assert_eq!(transport.sends, 1);
    }
}
