//! Root-owned durable state for the two-session admission manager.
//!
//! This is intentionally separate from a session `Writer`: admission routing
//! spans the current conversation and a possible empty sidechat, so one
//! session marker cannot be its authority.  A private root has exactly one
//! live manager owner, protected by `owner.lock`.  Each revision is written as
//! an immutable object before its small journal record is synced.  A torn last
//! journal line is ignored on recovery; a complete malformed record fails
//! closed.

use super::{StoreError, StoreResult, disk::Directory};
use crate::{
    conversation_state::content_hash,
    session_manager::{AdmissionError, SessionManagerV1},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const DIRECTORY: &str = "session-manager-v1";
const OWNER_LOCK: &str = "owner.lock";
const JOURNAL: &str = "commits.jsonl";
const SCHEMA_VERSION: u8 = 1;
/// A manager contains at most two sessions and 32 queued runs.  This cap
/// prevents a corrupted private file from turning recovery into an allocation
/// attack while leaving headroom for a complete durable admission ledger.
pub const MAX_SESSION_MANAGER_STATE_BYTES: usize = 512 * 1024;
/// Journal growth is bounded and fails closed.  A future compactor must use a
/// new atomic journal generation; it must never truncate the only recovery
/// record in place.
pub const MAX_SESSION_MANAGER_COMMITS: usize = 4096;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitV1 {
    schema_version: u8,
    revision: u64,
    state_name: String,
    content_hash: String,
}

/// A single exclusive owner of a root-scoped SessionManagerV1 ledger.
///
/// Callers must retain this value for the duration of an admission transition.
/// `persist` repeats the durable compare-and-swap even though the lock is
/// exclusive, so a stale engine snapshot cannot overwrite a newer decision.
#[derive(Debug)]
pub struct SessionManagerStore {
    directory: Arc<Directory>,
    lock: File,
}

impl Drop for SessionManagerStore {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

impl SessionManagerStore {
    /// Opens the manager below an already-private, canonical desktop root.
    /// No caller-supplied child paths are accepted.
    pub fn open_owned(root: &Path) -> StoreResult<Self> {
        let root = Arc::new(Directory::open_owned(root)?);
        Self::from_root(root)
    }

    pub(super) fn from_root(root: Arc<Directory>) -> StoreResult<Self> {
        root.verify()?;
        let directory = Arc::new(root.ensure_child(DIRECTORY)?);
        let lock = open_or_create(&directory, OWNER_LOCK)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(StoreError::Io(error)),
        }
        directory.verify()?;
        Ok(Self { directory, lock })
    }

    /// Reloads and validates the latest complete commit.  `None` means this
    /// root has not admitted an input yet.
    pub fn load(&self) -> StoreResult<Option<SessionManagerV1>> {
        self.verify_owner()?;
        let commits = self.read_commits()?;
        let Some(commit) = commits.last() else {
            return Ok(None);
        };
        let state = self.read_state(commit)?;
        Ok(Some(state))
    }

    /// Creates the initial revision exactly once.  Reopening and passing the
    /// same canonical initial state is idempotent; any other state is a CAS
    /// conflict rather than an overwrite.
    pub fn initialize(&self, initial: &SessionManagerV1) -> StoreResult<()> {
        self.verify_manager(initial)?;
        if initial.revision != 0 {
            return Err(StoreError::CasConflict("session manager initial revision"));
        }
        match self.load()? {
            None => self.append_commit(initial),
            Some(existing) if existing == *initial => Ok(()),
            Some(_) => Err(StoreError::CasConflict("session manager initialization")),
        }
    }

    /// Durably publishes `next` only when the recovered manager still has the
    /// caller's expected revision.  Equal revisions are allowed solely as an
    /// exact replay, preserving idempotent recovery without losing an update.
    pub fn persist(&self, expected_revision: u64, next: &SessionManagerV1) -> StoreResult<()> {
        self.verify_manager(next)?;
        let current = self.load()?.ok_or(StoreError::NotFound)?;
        if current.revision != expected_revision {
            return Err(StoreError::CasConflict("session manager"));
        }
        if next.revision < expected_revision {
            return Err(StoreError::CasConflict(
                "session manager revision regressed",
            ));
        }
        if next.revision == expected_revision {
            return if *next == current {
                Ok(())
            } else {
                Err(StoreError::CasConflict("session manager equal revision"))
            };
        }
        self.append_commit(next)
    }

    fn verify_owner(&self) -> StoreResult<()> {
        self.directory.verify()?;
        let current = self.directory.open(OWNER_LOCK, false, false)?;
        super::disk::same(&self.lock, &current)
    }

    fn verify_manager(&self, manager: &SessionManagerV1) -> StoreResult<()> {
        manager.validate().map_err(admission_restore_error)?;
        let bytes = canonical_state(manager)?;
        if bytes.len() > MAX_SESSION_MANAGER_STATE_BYTES {
            return Err(StoreError::Corrupt("session manager state size"));
        }
        Ok(())
    }

    fn append_commit(&self, manager: &SessionManagerV1) -> StoreResult<()> {
        self.verify_owner()?;
        let commits = self.read_commits()?;
        if commits.len() >= MAX_SESSION_MANAGER_COMMITS {
            return Err(StoreError::Overflow);
        }
        if let Some(previous) = commits.last()
            && manager.revision <= previous.revision
        {
            return Err(StoreError::CasConflict("session manager revision"));
        }
        let bytes = canonical_state(manager)?;
        if bytes.len() > MAX_SESSION_MANAGER_STATE_BYTES {
            return Err(StoreError::Corrupt("session manager state size"));
        }
        let state_name = state_name(manager.revision)?;
        write_immutable(&self.directory, &state_name, &bytes)?;
        let commit = CommitV1 {
            schema_version: SCHEMA_VERSION,
            revision: manager.revision,
            state_name,
            content_hash: content_hash(&bytes),
        };
        let mut line = serde_json::to_vec(&commit)?;
        line.push(b'\n');
        let mut journal = open_or_create(&self.directory, JOURNAL)?;
        let offset = journal.metadata()?.len();
        if offset > MAX_JOURNAL_BYTES as u64
            || offset
                .checked_add(line.len() as u64)
                .is_none_or(|length| length > MAX_JOURNAL_BYTES as u64)
        {
            return Err(StoreError::Overflow);
        }
        journal.seek(SeekFrom::Start(offset))?;
        journal.write_all(&line)?;
        journal.sync_all()?;
        self.directory.sync()
    }

    fn read_commits(&self) -> StoreResult<Vec<CommitV1>> {
        let mut journal = match self.directory.open(JOURNAL, false, false) {
            Ok(file) => file,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let length = journal.metadata()?.len();
        if length > MAX_JOURNAL_BYTES as u64 {
            return Err(StoreError::Corrupt("session manager journal size"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        journal.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length {
            return Err(StoreError::Corrupt("session manager journal length"));
        }
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let Some(position) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            // A crash may leave one incomplete record after the preceding
            // sync. It is not a commit and is intentionally ignored.
            return Ok(Vec::new());
        };
        if position == 0 {
            return Err(StoreError::Corrupt("session manager empty journal record"));
        }
        let complete = &bytes[..position];
        let mut commits = Vec::new();
        for line in complete.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                return Err(StoreError::Corrupt("session manager empty journal record"));
            }
            let commit: CommitV1 = serde_json::from_slice(line)?;
            validate_commit(&commit)?;
            if commits
                .last()
                .is_some_and(|previous: &CommitV1| commit.revision <= previous.revision)
            {
                return Err(StoreError::Corrupt("session manager journal revision"));
            }
            commits.push(commit);
            if commits.len() > MAX_SESSION_MANAGER_COMMITS {
                return Err(StoreError::Corrupt("session manager journal count"));
            }
        }
        Ok(commits)
    }

    fn read_state(&self, commit: &CommitV1) -> StoreResult<SessionManagerV1> {
        let mut state = self.directory.open(&commit.state_name, false, false)?;
        let length = state.metadata()?.len();
        if length == 0 || length > MAX_SESSION_MANAGER_STATE_BYTES as u64 {
            return Err(StoreError::Corrupt("session manager state size"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        state.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length || content_hash(&bytes) != commit.content_hash {
            return Err(StoreError::Corrupt("session manager state hash"));
        }
        let manager: SessionManagerV1 = serde_json::from_slice(&bytes)?;
        self.verify_manager(&manager)?;
        if manager.revision != commit.revision || canonical_state(&manager)? != bytes {
            return Err(StoreError::Corrupt("session manager state revision"));
        }
        Ok(manager)
    }
}

fn open_or_create(directory: &Directory, name: &str) -> StoreResult<File> {
    match directory.open(name, true, true) {
        Ok(file) => Ok(file),
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            directory.open(name, true, false)
        }
        Err(error) => Err(error),
    }
}

fn write_immutable(directory: &Directory, name: &str, bytes: &[u8]) -> StoreResult<()> {
    // A state object must never be visible under its committed name until its
    // complete bytes are synced. The journal can therefore ignore a crash
    // orphaned temporary file.
    let temporary = temporary_name();
    let mut file = directory.open(&temporary, true, true)?;
    let write = (|| -> StoreResult<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write {
        let _ = directory.unlink_file(&temporary);
        return Err(error);
    }
    let published = directory.link_no_replace(&temporary, name)?;
    let result = if published {
        directory.sync()
    } else {
        let mut existing = directory.open(name, false, false)?;
        let length = existing.metadata()?.len();
        if length != bytes.len() as u64 {
            Err(StoreError::Corrupt(
                "session manager immutable state conflict",
            ))
        } else {
            let mut found = Vec::with_capacity(length as usize);
            existing.read_to_end(&mut found)?;
            if found == bytes {
                Ok(())
            } else {
                Err(StoreError::Corrupt(
                    "session manager immutable state conflict",
                ))
            }
        }
    };
    let _ = directory.unlink_file(&temporary);
    result
}

fn temporary_name() -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(".session-manager-{}-{sequence}.tmp", std::process::id())
}

fn canonical_state(manager: &SessionManagerV1) -> StoreResult<Vec<u8>> {
    let bytes = serde_json::to_vec(manager)?;
    if bytes.is_empty() || bytes.len() > MAX_SESSION_MANAGER_STATE_BYTES {
        return Err(StoreError::Corrupt("session manager state size"));
    }
    Ok(bytes)
}

fn state_name(revision: u64) -> StoreResult<String> {
    // Decimal revisions keep object names bounded and cannot introduce a path
    // separator into Directory's capability-checked child API.
    let value = format!("state-{revision}.json");
    if value.len() > 64 {
        return Err(StoreError::Overflow);
    }
    Ok(value)
}

fn validate_commit(commit: &CommitV1) -> StoreResult<()> {
    if commit.schema_version != SCHEMA_VERSION
        || commit.state_name != state_name(commit.revision)?
        || commit.content_hash.len() != 64
        || !commit
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(StoreError::Corrupt("session manager journal record"));
    }
    Ok(())
}

fn admission_restore_error(error: AdmissionError) -> StoreError {
    let _ = error;
    StoreError::Corrupt("session manager state")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session_manager::{
        AdmissionState, DestinationMode, NextRunnableV1, SessionId, SubmissionPayloadV1,
        SubmitOutcomeV1, SubmitRequestV1,
    };
    use std::{fs, os::unix::fs::PermissionsExt};

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn current() -> SessionId {
        SessionId::new("current").unwrap()
    }

    fn manager() -> SessionManagerV1 {
        SessionManagerV1::new(current())
    }

    fn submit_and_queue(manager: &mut SessionManagerV1) {
        let session = manager.sessions.get(&current()).unwrap();
        let request = SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: "continue the current task".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: session.session_revision,
                expected_goal_revision: session.goal_revision,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted { admission, .. } =
            manager.submit(request, manager.revision).unwrap();
        let decided = manager
            .decide(
                &admission.id,
                manager.revision,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        let destination = match &prepared.state {
            AdmissionState::DestinationReady { session_id, .. } => session_id.clone(),
            _ => unreachable!(),
        };
        let appended = manager
            .record_input_appended(
                &prepared.id,
                manager.revision,
                prepared.revision,
                &destination,
            )
            .unwrap();
        manager
            .queue_run(&appended.id, manager.revision, appended.revision)
            .unwrap();
    }

    #[test]
    fn reopen_preserves_admission_queue_and_running_state() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut initial = manager();
        store.initialize(&initial).unwrap();
        submit_and_queue(&mut initial);
        store.persist(0, &initial).unwrap();
        let started = initial.start_next(initial.revision).unwrap();
        assert!(matches!(started, NextRunnableV1::Start { .. }));
        store.persist(initial.revision - 1, &initial).unwrap();
        drop(store);

        let reopened = SessionManagerStore::open_owned(&path).unwrap();
        let restored = reopened.load().unwrap().unwrap();
        assert_eq!(restored, initial);
        assert!(matches!(
            restored.next_runnable().unwrap(),
            NextRunnableV1::None
        ));
        assert!(
            restored
                .sessions
                .values()
                .any(|session| session.active_run.is_some())
        );
    }

    #[test]
    fn stale_cas_and_corrupt_latest_state_fail_closed() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut state = manager();
        store.initialize(&state).unwrap();
        submit_and_queue(&mut state);
        store.persist(0, &state).unwrap();
        assert!(matches!(
            store.persist(0, &state),
            Err(StoreError::CasConflict(_))
        ));
        drop(store);

        let state_path = path
            .join(DIRECTORY)
            .join(state_name(state.revision).unwrap());
        fs::write(state_path, b"{}").unwrap();
        let reopened = SessionManagerStore::open_owned(&path).unwrap();
        assert!(matches!(reopened.load(), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn owner_lock_and_partial_journal_preserve_last_complete_commit() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        store.initialize(&manager()).unwrap();
        assert!(matches!(
            SessionManagerStore::open_owned(&path),
            Err(StoreError::Busy)
        ));
        drop(store);

        let journal = path.join(DIRECTORY).join(JOURNAL);
        let mut file = fs::OpenOptions::new().append(true).open(journal).unwrap();
        file.write_all(br#"{\"schema_version\":1"#).unwrap();
        file.sync_all().unwrap();
        let reopened = SessionManagerStore::open_owned(&path).unwrap();
        assert_eq!(reopened.load().unwrap().unwrap(), manager());
    }
}
