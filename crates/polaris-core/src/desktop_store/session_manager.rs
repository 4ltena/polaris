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
    session_manager::{
        AdmissionDestination, AdmissionError, AdmissionId, AdmissionState, AdmissionV1,
        ContextRelation, RouteReason, RoutingJudgementV1, RunId, SessionInitV1, SessionManagerV1,
        SubmitRequestV1, TerminalStatus, sidechat_session_id,
    },
};
use serde::{Deserialize, Serialize};
#[path = "session_manager_family.rs"]
pub(super) mod family;
#[path = "session_manager_generation.rs"]
mod generation;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "session_manager_migration.rs"]
pub(super) mod migration;
#[cfg(all(test, windows))]
use polaris_auth::protection::WindowsFamilyFile as File;
#[cfg(not(all(test, windows)))]
use std::fs::File;
use std::{
    cell::Cell,
    collections::BTreeMap,
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
const MAX_COMMIT_RECORD_BYTES: usize = 182;
const MAX_GENERATION_COMMIT_RECORD_BYTES: usize = 273;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitV1 {
    schema_version: u8,
    revision: u64,
    state_name: String,
    content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_commit_hash: Option<String>,
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
    legacy_verified: Cell<bool>,
    generation_verified: Cell<bool>,
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
        let store = Self {
            directory,
            lock,
            legacy_verified: Cell::new(false),
            generation_verified: Cell::new(false),
        };
        store.rescue_if_needed()?;
        if store.read_pointer()?.is_some() {
            let active = store.generation_directory()?;
            store.repair_torn_tail(&active)?;
        } else {
            store.repair_torn_tail(&store.directory)?;
        }
        Ok(store)
    }

    /// Reloads and validates the latest complete commit.  `None` means this
    /// root has not admitted an input yet.
    pub fn load(&self) -> StoreResult<Option<SessionManagerV1>> {
        self.verify_owner()?;
        if self.read_pointer()?.is_some() {
            return self.load_generation().map(Some);
        }
        self.load_legacy()
    }

    fn load_legacy(&self) -> StoreResult<Option<SessionManagerV1>> {
        let commits = self.read_commits()?;
        self.verify_legacy_evidence(&commits)?;
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
            None => {
                self.check_capacity(0, 0, serde_json::to_vec(initial)?.len(), initial, false)?;
                self.append_commit(initial)
            }
            Some(existing) if existing == *initial => Ok(()),
            Some(_) => Err(StoreError::CasConflict("session manager initialization")),
        }
    }

    /// Durably publishes `next` only when the recovered manager still has the
    /// caller's expected revision.  Equal revisions are allowed solely as an
    /// exact replay, preserving idempotent recovery without losing an update.
    pub fn persist(&self, expected_revision: u64, next: &SessionManagerV1) -> StoreResult<()> {
        self.persist_checked(expected_revision, next, None, None)
    }

    pub fn persist_observed(
        &self,
        expected_revision: u64,
        next: &SessionManagerV1,
        session: &crate::session_manager::SessionId,
        session_revision: u64,
        goal_revision: u64,
    ) -> StoreResult<()> {
        self.persist_checked(
            expected_revision,
            next,
            None,
            Some((session, session_revision, goal_revision)),
        )
    }

    /// The submission owner supplies receipt counts from the durable session
    /// snapshots while holding the manager owner lock, before any raw append.
    pub fn persist_submission(
        &self,
        expected_revision: u64,
        next: &SessionManagerV1,
        receipts: &BTreeMap<crate::session_manager::SessionId, usize>,
    ) -> StoreResult<()> {
        self.persist_checked(expected_revision, next, Some(receipts), None)
    }

    pub fn persist_observed_submission(
        &self,
        expected_revision: u64,
        next: &SessionManagerV1,
        receipts: &BTreeMap<crate::session_manager::SessionId, usize>,
        session: &crate::session_manager::SessionId,
        session_revision: u64,
        goal_revision: u64,
    ) -> StoreResult<()> {
        self.persist_checked(
            expected_revision,
            next,
            Some(receipts),
            Some((session, session_revision, goal_revision)),
        )
    }

    fn persist_checked(
        &self,
        expected_revision: u64,
        next: &SessionManagerV1,
        receipts: Option<&BTreeMap<crate::session_manager::SessionId, usize>>,
        observed: Option<(&crate::session_manager::SessionId, u64, u64)>,
    ) -> StoreResult<()> {
        self.verify_manager(next)?;
        let current = self.load()?.ok_or(StoreError::NotFound)?;
        if current.generation != next.generation {
            return Err(StoreError::CasConflict("session manager generation"));
        }
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
        validate_persist_transition(&current, next, observed)?;
        let new_submit = next
            .admissions
            .keys()
            .any(|id| !current.admissions.contains_key(id));
        if new_submit {
            let receipts = receipts.ok_or(StoreError::Capacity("receipt evidence missing"))?;
            if current.sessions.keys().any(|id| !receipts.contains_key(id)) {
                return Err(StoreError::Capacity("receipt evidence incomplete"));
            }
            next.check_receipt_capacity(receipts)
                .map_err(|_| StoreError::Capacity("admission receipts"))?;
        }
        if current.generation != 0 {
            return self.persist_generation(&current, next, new_submit);
        }
        let commits = self.read_commits()?;
        let journal_len = self.journal_len()?;
        self.check_capacity(
            commits.len(),
            journal_len,
            serde_json::to_vec(next)?.len(),
            next,
            new_submit,
        )?;
        self.append_commit(next)
    }

    fn journal_len(&self) -> StoreResult<usize> {
        Self::journal_len_in(&self.directory)
    }

    fn journal_len_in(directory: &Directory) -> StoreResult<usize> {
        match directory.open(JOURNAL, false, false) {
            Ok(file) => usize::try_from(file.metadata()?.len()).map_err(|_| StoreError::Overflow),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn repair_torn_tail(&self, directory: &Directory) -> StoreResult<()> {
        let mut journal = match directory.open(JOURNAL, true, false) {
            Ok(file) => file,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        journal.read_to_end(&mut bytes)?;
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |pos| pos + 1);
        if complete < bytes.len() {
            journal.set_len(complete as u64)?;
            journal.sync_all()?;
            directory.sync()?;
        }
        Ok(())
    }

    fn check_capacity(
        &self,
        commits: usize,
        journal_len: usize,
        serialized_len: usize,
        next: &SessionManagerV1,
        new_submit: bool,
    ) -> StoreResult<()> {
        let (pending_commits, pending_growth) = next.remaining_capacity_reservation();
        let required_commits = if new_submit {
            pending_commits.saturating_add(16).max(320)
        } else {
            pending_commits
        };
        let growth = pending_growth + if new_submit { 4096 } else { 0 };
        let record_size = if next.generation == 0 {
            MAX_COMMIT_RECORD_BYTES
        } else {
            MAX_GENERATION_COMMIT_RECORD_BYTES
        };
        if commits.saturating_add(1).saturating_add(required_commits) > MAX_SESSION_MANAGER_COMMITS
        {
            return Err(StoreError::Capacity("commit reserve"));
        }
        if journal_len
            .saturating_add((required_commits.saturating_add(1)).saturating_mul(record_size))
            > MAX_JOURNAL_BYTES
        {
            return Err(StoreError::Capacity("journal reserve"));
        }
        if serialized_len.saturating_add(growth) > MAX_SESSION_MANAGER_STATE_BYTES {
            return Err(StoreError::Capacity("state reserve"));
        }
        Ok(())
    }

    fn verify_owner(&self) -> StoreResult<()> {
        self.directory.verify()?;
        let current = self.directory.open(OWNER_LOCK, false, false)?;
        super::disk::same(&self.lock, &current)
    }

    fn verify_manager(&self, manager: &SessionManagerV1) -> StoreResult<()> {
        manager.validate().map_err(admission_restore_error)?;
        if manager.generation == 0
            && canonical_state(manager)?.len() > MAX_SESSION_MANAGER_STATE_BYTES
        {
            return Err(StoreError::Capacity("state size"));
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
            return Err(StoreError::Capacity("state size"));
        }
        let state_name = state_name(manager.revision)?;
        write_immutable(&self.directory, &state_name, &bytes)?;
        let commit = CommitV1 {
            schema_version: SCHEMA_VERSION,
            revision: manager.revision,
            state_name,
            content_hash: content_hash(&bytes),
            previous_commit_hash: None,
        };
        let mut line = serde_json::to_vec(&commit)?;
        line.push(b'\n');
        if line.len() > MAX_COMMIT_RECORD_BYTES {
            return Err(StoreError::Capacity("commit record size"));
        }
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
        Self::read_commits_from(&self.directory, SCHEMA_VERSION, None)
    }

    fn read_commits_from(
        directory: &Directory,
        schema_version: u8,
        first_hash: Option<&str>,
    ) -> StoreResult<Vec<CommitV1>> {
        let mut journal = match directory.open(JOURNAL, false, false) {
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
        Read::by_ref(&mut journal)
            .take(length + 1)
            .read_to_end(&mut bytes)?;
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
            let maximum = if schema_version == SCHEMA_VERSION {
                MAX_COMMIT_RECORD_BYTES
            } else {
                MAX_GENERATION_COMMIT_RECORD_BYTES
            };
            if line.is_empty() || line.len() >= maximum {
                return Err(StoreError::Corrupt("session manager empty journal record"));
            }
            let commit: CommitV1 = serde_json::from_slice(line)?;
            validate_commit(&commit, schema_version)?;
            if serde_json::to_vec(&commit)? != line {
                return Err(StoreError::Corrupt(
                    "session manager journal canonical form",
                ));
            }
            if schema_version != SCHEMA_VERSION {
                let expected = match commits.last() {
                    Some(previous) => Some(content_hash(&serde_json::to_vec(previous)?)),
                    None => first_hash.map(str::to_owned),
                };
                if commit.previous_commit_hash != expected {
                    return Err(StoreError::Corrupt(
                        "session manager generation journal chain",
                    ));
                }
            }
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
        Read::by_ref(&mut state)
            .take(length + 1)
            .read_to_end(&mut bytes)?;
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

    fn verify_legacy_evidence(&self, commits: &[CommitV1]) -> StoreResult<()> {
        if !self.legacy_verified.get() {
            for commit in commits {
                self.read_state(commit)?;
            }
            self.legacy_verified.set(true);
        }
        Ok(())
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
    if bytes.is_empty() {
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

fn validate_commit(commit: &CommitV1, schema_version: u8) -> StoreResult<()> {
    if commit.schema_version != schema_version
        || commit.state_name != state_name(commit.revision)?
        || commit.content_hash.len() != 64
        || !commit
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || (schema_version == SCHEMA_VERSION && commit.previous_commit_hash.is_some())
        || (schema_version != SCHEMA_VERSION
            && commit.previous_commit_hash.as_ref().is_none_or(|hash| {
                hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            }))
    {
        return Err(StoreError::Corrupt("session manager journal record"));
    }
    Ok(())
}

fn validate_persist_transition(
    current: &SessionManagerV1,
    next: &SessionManagerV1,
    observed: Option<(&crate::session_manager::SessionId, u64, u64)>,
) -> StoreResult<()> {
    if current.new_submit_closed != next.new_submit_closed {
        return Err(StoreError::CasConflict("session manager submit closure"));
    }
    let mut added = 0;
    for (id, prior) in &current.admissions {
        let candidate = next
            .admissions
            .get(id)
            .ok_or(StoreError::CasConflict("admission removed"))?;
        if prior.id != candidate.id
            || prior.parent_session != candidate.parent_session
            || prior.payload != candidate.payload
            || prior.payload_hash != candidate.payload_hash
        {
            return Err(StoreError::CasConflict("admission identity changed"));
        }
        if prior == candidate {
            continue;
        }
        if candidate.revision != prior.revision.checked_add(1).ok_or(StoreError::Overflow)?
            || prior.state == candidate.state
            || !admission_transition_allowed(&prior.state, &candidate.state)
            || !admission_binding_allowed(prior, candidate, next)
        {
            return Err(StoreError::CasConflict("admission transition"));
        }
        let expected_reason = match &candidate.state {
            AdmissionState::Decided { reason, .. }
            | AdmissionState::DestinationReady { reason, .. } => Some(*reason),
            _ => prior.route_reason,
        };
        if candidate.route_reason != expected_reason {
            return Err(StoreError::CasConflict("admission route reason"));
        }
    }
    for (id, candidate) in &next.admissions {
        if !current.admissions.contains_key(id) {
            added += 1;
            if candidate.revision != 0
                || candidate.state != AdmissionState::Reserved
                || candidate.route_reason.is_some()
            {
                return Err(StoreError::CasConflict("new admission state"));
            }
        }
    }
    if added > 1 || (current.new_submit_closed && added != 0) {
        return Err(StoreError::CasConflict("new admission count"));
    }
    if !replays_exactly(current, next, observed) {
        return Err(StoreError::CasConflict("session manager transition"));
    }
    Ok(())
}

/// Reproduce one owner operation (optionally preceded by one observed writer
/// revision) and compare the entire manager. This also checks FIFO order,
/// session revisions, active runs, sidechat creation/removal, and routing.
fn replays_exactly(
    current: &SessionManagerV1,
    next: &SessionManagerV1,
    observed: Option<(&crate::session_manager::SessionId, u64, u64)>,
) -> bool {
    let mut bases = vec![current.clone()];
    if let Some((id, session_revision, goal_revision)) = observed {
        let mut observed_state = current.clone();
        if observed_state
            .observe_session_revisions(id, current.revision, session_revision, goal_revision)
            .is_ok()
            && observed_state != *current
        {
            bases.push(observed_state);
        }
    }
    for base in bases {
        if base == *next {
            return true;
        }
        if let Some(admission) = next
            .admissions
            .values()
            .find(|admission| !base.admissions.contains_key(&admission.id))
        {
            let request = SubmitRequestV1 {
                parent_session: admission.parent_session.clone(),
                payload: admission.payload.clone(),
                admission_id: Some(admission.id.clone()),
            };
            if transition_matches(&base, next, |state| {
                state.submit(request, state.revision).map(|_| ())
            }) {
                return true;
            }
        }
        if transition_matches(&base, next, |state| state.recover_classifying().map(|_| ())) {
            return true;
        }
        for prior in base.admissions.values() {
            let Some(candidate) = next.admissions.get(&prior.id) else {
                continue;
            };
            if prior == candidate {
                continue;
            }
            if replays_admission_operation(&base, next, &prior.id, prior.revision, &candidate.state)
            {
                return true;
            }
        }
    }
    false
}

fn transition_matches(
    base: &SessionManagerV1,
    next: &SessionManagerV1,
    operation: impl FnOnce(&mut SessionManagerV1) -> Result<(), AdmissionError>,
) -> bool {
    let mut replay = base.clone();
    operation(&mut replay).is_ok() && replay == *next
}

fn replays_admission_operation(
    base: &SessionManagerV1,
    next: &SessionManagerV1,
    id: &AdmissionId,
    admission_revision: u64,
    candidate: &AdmissionState,
) -> bool {
    let manager_revision = base.revision;
    match candidate {
        AdmissionState::Classifying => transition_matches(base, next, |state| {
            state
                .begin_classification(id, manager_revision, admission_revision)
                .map(|_| ())
        }),
        AdmissionState::Decided {
            reason: RouteReason::ContextChanged,
            ..
        }
        | AdmissionState::DestinationReady {
            reason: RouteReason::ContextChanged,
            ..
        } => {
            transition_matches(base, next, |state| {
                state
                    .decide_context_changed(id, manager_revision, admission_revision)
                    .map(|_| ())
            }) || (matches!(candidate, AdmissionState::DestinationReady { .. })
                && transition_matches(base, next, |state| {
                    state
                        .prepare_destination(id, manager_revision, admission_revision)
                        .map(|_| ())
                }))
        }
        AdmissionState::Decided { reason, .. } => {
            let (judgement, approval_reply, cancelled) = judgement_for_reason(*reason);
            transition_matches(base, next, |state| {
                state
                    .decide(
                        id,
                        manager_revision,
                        admission_revision,
                        judgement,
                        approval_reply,
                        cancelled,
                    )
                    .map(|_| ())
            })
        }
        AdmissionState::DestinationReady { .. } => transition_matches(base, next, |state| {
            state
                .prepare_destination(id, manager_revision, admission_revision)
                .map(|_| ())
        }),
        AdmissionState::InputAppended { session_id } => {
            transition_matches(base, next, |state| {
                state
                    .record_input_appended(id, manager_revision, admission_revision, session_id)
                    .map(|_| ())
            }) || transition_matches(base, next, |state| {
                state
                    .recover_input_appended(id, manager_revision, admission_revision, session_id)
                    .map(|_| ())
            })
        }
        AdmissionState::RunQueued { .. } => transition_matches(base, next, |state| {
            state
                .queue_run(id, manager_revision, admission_revision)
                .map(|_| ())
        }),
        AdmissionState::Running { .. } => transition_matches(base, next, |state| {
            state.start_next(manager_revision).map(|_| ())
        }),
        AdmissionState::Terminal { status, .. } => {
            (matches!(status, TerminalStatus::Cancelled)
                && transition_matches(base, next, |state| {
                    state
                        .cancel(id, manager_revision, admission_revision)
                        .map(|_| ())
                }))
                || (matches!(
                    status,
                    TerminalStatus::Failed | TerminalStatus::OutcomeUnknown
                ) && transition_matches(base, next, |state| {
                    state
                        .fail_preflight(id, manager_revision, admission_revision, status.clone())
                        .map(|_| ())
                }))
                || (matches!(status, TerminalStatus::OutcomeUnknown)
                    && transition_matches(base, next, |state| {
                        state
                            .recover_unstarted_committed(id, manager_revision, admission_revision)
                            .map(|_| ())
                    }))
                || transition_matches(base, next, |state| {
                    state
                        .terminal(id, manager_revision, admission_revision, status.clone())
                        .map(|_| ())
                })
        }
        AdmissionState::Reserved => false,
    }
}

fn judgement_for_reason(reason: RouteReason) -> (Option<RoutingJudgementV1>, bool, bool) {
    let judgement = |relation, probability| {
        Some(RoutingJudgementV1 {
            relation,
            probability: Some(probability),
            confidence: Some(1.0),
        })
    };
    match reason {
        RouteReason::ApprovalReply => (None, true, false),
        RouteReason::Cancelled => (None, false, true),
        RouteReason::SameContext => (judgement(ContextRelation::SameContext, 1.0), false, false),
        RouteReason::DistinctContext => (
            judgement(ContextRelation::DistinctContext, 1.0),
            false,
            false,
        ),
        RouteReason::Ambiguous => (judgement(ContextRelation::Unclear, 0.5), false, false),
        RouteReason::ClassifierInvalid => {
            (judgement(ContextRelation::SameContext, 2.0), false, false)
        }
        _ => (None, false, false),
    }
}

fn admission_transition_allowed(before: &AdmissionState, after: &AdmissionState) -> bool {
    use AdmissionState as S;
    matches!(
        (before, after),
        (S::Reserved, S::Classifying)
            | (S::Reserved, S::Decided { .. })
            | (S::Classifying, S::Decided { .. })
            | (S::Decided { .. }, S::DestinationReady { .. })
            | (S::DestinationReady { .. }, S::InputAppended { .. })
            | (S::InputAppended { .. }, S::RunQueued { .. })
            | (S::RunQueued { .. }, S::Running { .. })
            | (
                S::Decided { .. },
                S::Decided {
                    reason: RouteReason::ContextChanged,
                    ..
                },
            )
            | (
                S::DestinationReady { .. },
                S::DestinationReady {
                    reason: RouteReason::ContextChanged,
                    ..
                },
            )
            | (
                S::Reserved
                    | S::Classifying
                    | S::Decided { .. }
                    | S::DestinationReady { .. }
                    | S::InputAppended { .. }
                    | S::RunQueued { .. }
                    | S::Running { .. },
                S::Terminal {
                    status: TerminalStatus::Cancelled,
                    ..
                },
            )
            | (
                S::DestinationReady { .. },
                S::Terminal {
                    status: TerminalStatus::Failed | TerminalStatus::OutcomeUnknown,
                    ..
                },
            )
            | (
                S::InputAppended { .. } | S::RunQueued { .. },
                S::Terminal {
                    status: TerminalStatus::OutcomeUnknown,
                    ..
                },
            )
            | (S::Running { .. }, S::Terminal { .. })
    )
}

fn admission_binding_allowed(
    before: &AdmissionV1,
    after: &AdmissionV1,
    manager: &SessionManagerV1,
) -> bool {
    use AdmissionState as S;
    match (&before.state, &after.state) {
        (
            S::Decided {
                destination: AdmissionDestination::Current,
                ..
            },
            S::DestinationReady { session_id, .. },
        ) => session_id == &before.parent_session,
        (
            S::Decided {
                destination: AdmissionDestination::New,
                ..
            },
            S::DestinationReady { session_id, .. },
        ) => {
            session_id == &sidechat_session_id(&before.parent_session, &before.id)
                && matches!(manager.sessions.get(session_id).map(|session| &session.init), Some(SessionInitV1::EmptySidechat { created_from_admission_id, parent_session, .. }) if created_from_admission_id == &before.id && parent_session == &before.parent_session)
        }
        (
            S::DestinationReady { .. },
            S::DestinationReady {
                session_id,
                reason: RouteReason::ContextChanged,
            },
        ) => session_id == &before.parent_session,
        (
            S::DestinationReady {
                session_id: prior, ..
            },
            S::InputAppended { session_id },
        ) => session_id == prior,
        (S::InputAppended { session_id: prior }, S::RunQueued { session_id, run_id }) => {
            session_id == prior && run_id == &RunId::derive(&before.id)
        }
        (
            S::RunQueued {
                session_id: prior,
                run_id: prior_run,
            },
            S::Running { session_id, run_id },
        ) => session_id == prior && run_id == prior_run,
        (
            S::Running {
                session_id: prior,
                run_id: prior_run,
            },
            S::Terminal {
                session_id, run_id, ..
            },
        ) => session_id.as_ref() == Some(prior) && run_id.as_ref() == Some(prior_run),
        (
            S::InputAppended { session_id: prior },
            S::Terminal {
                session_id, run_id, ..
            },
        ) => session_id.as_ref() == Some(prior) && run_id.is_none(),
        (
            S::RunQueued {
                session_id: prior,
                run_id: prior_run,
            },
            S::Terminal {
                session_id, run_id, ..
            },
        ) => session_id.as_ref() == Some(prior) && run_id.as_ref() == Some(prior_run),
        (
            S::DestinationReady {
                session_id: prior, ..
            },
            S::Terminal {
                session_id, run_id, ..
            },
        ) => session_id.as_ref().is_none_or(|id| id == prior) && run_id.is_none(),
        (
            S::Reserved | S::Classifying | S::Decided { .. },
            S::Terminal {
                session_id, run_id, ..
            },
        ) => session_id.is_none() && run_id.is_none(),
        (
            S::Reserved | S::Classifying | S::Decided { .. },
            S::Decided {
                destination: AdmissionDestination::Current,
                reason: RouteReason::ContextChanged,
            },
        ) => true,
        (
            _,
            S::Decided {
                reason: RouteReason::ContextChanged,
                ..
            },
        ) => false,
        _ => true,
    }
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
    use std::{
        fs,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };

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

    #[test]
    fn maximum_commit_record_fits_the_reserved_byte_bound() {
        let record = CommitV1 {
            schema_version: SCHEMA_VERSION,
            revision: u64::MAX,
            state_name: state_name(u64::MAX).unwrap(),
            content_hash: "f".repeat(64),
            previous_commit_hash: None,
        };
        assert!(serde_json::to_vec(&record).unwrap().len() < MAX_COMMIT_RECORD_BYTES);
        let generation = CommitV1 {
            schema_version: 2,
            previous_commit_hash: Some("f".repeat(64)),
            ..record
        };
        assert!(
            serde_json::to_vec(&generation).unwrap().len() < MAX_GENERATION_COMMIT_RECORD_BYTES
        );
    }

    #[test]
    fn persist_rejects_missing_or_full_receipt_evidence_and_admission_rewrites() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let initial = manager();
        store.initialize(&initial).unwrap();
        let mut candidate = initial.clone();
        let request = SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: "accepted".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted { admission, .. } = candidate.submit(request, 0).unwrap();
        assert!(matches!(
            store.persist(0, &candidate),
            Err(StoreError::Capacity("receipt evidence missing"))
        ));
        assert!(matches!(
            store.persist_submission(0, &candidate, &BTreeMap::new()),
            Err(StoreError::Capacity("receipt evidence incomplete"))
        ));
        assert!(matches!(
            store.persist_submission(0, &candidate, &BTreeMap::from([(current(), 4096)])),
            Err(StoreError::Capacity("admission receipts"))
        ));
        store
            .persist_submission(0, &candidate, &BTreeMap::from([(current(), 4095)]))
            .unwrap();
        let mut removed = candidate.clone();
        removed.admissions.clear();
        removed.queue.clear();
        removed.revision += 1;
        assert!(matches!(
            store.persist(candidate.revision, &removed),
            Err(StoreError::CasConflict("admission removed"))
        ));
        let mut replaced = candidate.clone();
        replaced
            .admissions
            .get_mut(&admission.id)
            .unwrap()
            .payload
            .text = "replacement".into();
        let replacement_hash = replaced.admissions[&admission.id]
            .payload
            .payload_hash()
            .unwrap();
        replaced
            .admissions
            .get_mut(&admission.id)
            .unwrap()
            .payload_hash = replacement_hash;
        replaced.revision += 1;
        assert!(matches!(
            store.persist(candidate.revision, &replaced),
            Err(StoreError::CasConflict("admission identity changed"))
        ));
        let mut terminal = candidate.clone();
        terminal
            .cancel(&admission.id, terminal.revision, admission.revision)
            .unwrap();
        store.persist(candidate.revision, &terminal).unwrap();
        let mut rollback = terminal.clone();
        let id = admission.id.clone();
        rollback.admissions.insert(id.clone(), admission);
        rollback.queue.push(id);
        rollback.revision += 1;
        assert!(matches!(
            store.persist(terminal.revision, &rollback),
            Err(StoreError::CasConflict(_))
        ));
    }

    #[test]
    fn persisted_new_route_checks_both_parent_and_sidechat_receipts() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut state = manager();
        store.initialize(&state).unwrap();
        let request = SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: "separate".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::New,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted { admission, .. } = state.submit(request, 0).unwrap();
        let sidechat = sidechat_session_id(&current(), &admission.id);
        let counts = BTreeMap::from([(current(), 4095), (sidechat.clone(), 4096)]);
        assert!(matches!(
            store.persist_submission(0, &state, &counts),
            Err(StoreError::Capacity("admission receipts"))
        ));
        let counts = BTreeMap::from([(current(), 4095), (sidechat, 4095)]);
        store.persist_submission(0, &state, &counts).unwrap();
    }

    #[test]
    fn outcome_unknown_cannot_be_removed_from_a_committed_manager() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut state = manager();
        store.initialize(&state).unwrap();
        let request = SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: "uncertain".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted { admission, .. } = state.submit(request, 0).unwrap();
        store
            .persist_submission(0, &state, &BTreeMap::from([(current(), 0)]))
            .unwrap();
        let before = state.revision;
        let decided = state
            .decide(
                &admission.id,
                before,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        store.persist(before, &state).unwrap();
        let before = state.revision;
        let ready = state
            .prepare_destination(&admission.id, before, decided.revision)
            .unwrap();
        store.persist(before, &state).unwrap();
        let before = state.revision;
        state
            .fail_preflight(
                &admission.id,
                before,
                ready.revision,
                TerminalStatus::OutcomeUnknown,
            )
            .unwrap();
        store.persist(before, &state).unwrap();
        let mut omitted = state.clone();
        omitted.admissions.clear();
        omitted.revision += 1;
        assert!(matches!(
            store.persist(state.revision, &omitted),
            Err(StoreError::CasConflict("admission removed"))
        ));
    }

    #[test]
    fn persist_replays_fifo_routing_and_session_mutations() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut state = manager();
        store.initialize(&state).unwrap();
        for text in ["first", "second"] {
            let request = SubmitRequestV1 {
                parent_session: current(),
                payload: SubmissionPayloadV1 {
                    text: text.into(),
                    attachment_ids: Vec::new(),
                    route: DestinationMode::Current,
                    expected_session_revision: 0,
                    expected_goal_revision: 0,
                },
                admission_id: None,
            };
            let before = state.revision;
            state.submit(request, before).unwrap();
            store
                .persist_submission(before, &state, &BTreeMap::from([(current(), 0)]))
                .unwrap();
        }
        let mut reordered = state.clone();
        reordered.queue.swap(0, 1);
        reordered.revision += 1;
        assert!(matches!(
            store.persist(state.revision, &reordered),
            Err(StoreError::CasConflict("session manager transition"))
        ));
        let mut forged_revision = state.clone();
        forged_revision
            .sessions
            .get_mut(&current())
            .unwrap()
            .session_revision = 100;
        forged_revision.revision += 1;
        assert!(matches!(
            store.persist(state.revision, &forged_revision),
            Err(StoreError::CasConflict("session manager transition"))
        ));
        let mut orphan_run = state.clone();
        orphan_run.sessions.get_mut(&current()).unwrap().active_run =
            Some(RunId::derive(&state.queue[0]));
        orphan_run.revision += 1;
        assert!(matches!(
            store.persist(state.revision, &orphan_run),
            Err(StoreError::CasConflict("session manager transition"))
        ));
        let mut wrong_route = state.clone();
        let id = state.queue[0].clone();
        let changed = wrong_route.admissions.get_mut(&id).unwrap();
        changed.state = AdmissionState::Decided {
            destination: AdmissionDestination::New,
            reason: RouteReason::ExplicitNew,
        };
        changed.route_reason = Some(RouteReason::ExplicitNew);
        changed.revision += 1;
        wrong_route.revision += 1;
        assert!(matches!(
            store.persist(state.revision, &wrong_route),
            Err(StoreError::CasConflict("session manager transition"))
        ));
        let mut observed = state.clone();
        observed
            .observe_session_revisions(&current(), state.revision, 100, 0)
            .unwrap();
        store
            .persist_observed(state.revision, &observed, &current(), 100, 0)
            .unwrap();
    }

    fn full_legacy_fixture(path: &Path) -> Vec<u8> {
        let directory = path.join(DIRECTORY);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal_bytes = Vec::new();
        for revision in 0..MAX_SESSION_MANAGER_COMMITS as u64 {
            let mut state = manager();
            if revision == (MAX_SESSION_MANAGER_COMMITS - 1) as u64 {
                state.revision = revision - 1;
                let request = SubmitRequestV1 {
                    parent_session: current(),
                    payload: SubmissionPayloadV1 {
                        text: "accepted before capacity repair".into(),
                        attachment_ids: Vec::new(),
                        route: DestinationMode::Current,
                        expected_session_revision: 0,
                        expected_goal_revision: 0,
                    },
                    admission_id: None,
                };
                state.submit(request, revision - 1).unwrap();
            } else {
                state.revision = revision;
            }
            let bytes = serde_json::to_vec(&state).unwrap();
            let name = state_name(revision).unwrap();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join(&name))
                .unwrap();
            file.write_all(&bytes).unwrap();
            let commit = CommitV1 {
                schema_version: SCHEMA_VERSION,
                revision,
                state_name: name,
                content_hash: content_hash(&bytes),
                previous_commit_hash: None,
            };
            let mut line = serde_json::to_vec(&commit).unwrap();
            line.push(b'\n');
            journal_bytes.extend(line);
        }
        fs::write(directory.join(JOURNAL), &journal_bytes).unwrap();
        fs::set_permissions(directory.join(JOURNAL), fs::Permissions::from_mode(0o600)).unwrap();
        journal_bytes
    }

    fn submit_and_queue(store: &SessionManagerStore, manager: &mut SessionManagerV1) {
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
        let before = manager.revision;
        let SubmitOutcomeV1::Accepted { admission, .. } =
            manager.submit(request, manager.revision).unwrap();
        store
            .persist_submission(before, manager, &BTreeMap::from([(current(), 0)]))
            .unwrap();
        let before = manager.revision;
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
        store.persist(before, manager).unwrap();
        let before = manager.revision;
        let prepared = manager
            .prepare_destination(&decided.id, manager.revision, decided.revision)
            .unwrap();
        store.persist(before, manager).unwrap();
        let destination = match &prepared.state {
            AdmissionState::DestinationReady { session_id, .. } => session_id.clone(),
            _ => unreachable!(),
        };
        let before = manager.revision;
        let appended = manager
            .record_input_appended(
                &prepared.id,
                manager.revision,
                prepared.revision,
                &destination,
            )
            .unwrap();
        store.persist(before, manager).unwrap();
        let before = manager.revision;
        manager
            .queue_run(&appended.id, manager.revision, appended.revision)
            .unwrap();
        store.persist(before, manager).unwrap();
    }

    #[test]
    fn reopen_preserves_admission_queue_and_running_state() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut initial = manager();
        store.initialize(&initial).unwrap();
        submit_and_queue(&store, &mut initial);
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
        submit_and_queue(&store, &mut state);
        assert!(matches!(
            store.persist(0, &state),
            Err(StoreError::CasConflict(_))
        ));
        drop(store);

        let state_path = path
            .join(DIRECTORY)
            .join(state_name(state.revision).unwrap());
        fs::write(state_path, b"{}").unwrap();
        assert!(matches!(
            SessionManagerStore::open_owned(&path),
            Err(StoreError::Corrupt(_))
        ));
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
        let mut next = manager();
        submit_and_queue(&reopened, &mut next);
        assert_eq!(reopened.load().unwrap().unwrap(), next);
    }

    #[test]
    fn legacy_open_rejects_missing_middle_state_before_repairing_tail() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut state = manager();
        store.initialize(&state).unwrap();
        let request = SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: "retain all evidence".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted { admission, .. } = state.submit(request, 0).unwrap();
        store
            .persist_submission(0, &state, &BTreeMap::from([(current(), 0)]))
            .unwrap();
        let before = state.revision;
        state
            .decide(
                &admission.id,
                before,
                admission.revision,
                None,
                false,
                false,
            )
            .unwrap();
        store.persist(before, &state).unwrap();
        drop(store);
        let directory = path.join(DIRECTORY);
        let journal_path = directory.join(JOURNAL);
        let mut journal = fs::read(&journal_path).unwrap();
        journal.extend_from_slice(b"unfinished-tail");
        fs::write(&journal_path, &journal).unwrap();
        fs::remove_file(directory.join(state_name(1).unwrap())).unwrap();
        assert!(SessionManagerStore::open_owned(&path).is_err());
        assert_eq!(fs::read(&journal_path).unwrap(), journal);
    }

    #[test]
    fn full_legacy_with_reserved_input_rescues_without_changing_old_evidence() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let directory = path.join(DIRECTORY);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut journal_bytes = Vec::new();
        let mut pending_id = None;
        for revision in 0..MAX_SESSION_MANAGER_COMMITS as u64 {
            let mut state = manager();
            if revision == (MAX_SESSION_MANAGER_COMMITS - 1) as u64 {
                state.revision = revision - 1;
                let request = SubmitRequestV1 {
                    parent_session: current(),
                    payload: SubmissionPayloadV1 {
                        text: "accepted before capacity repair".into(),
                        attachment_ids: Vec::new(),
                        route: DestinationMode::Current,
                        expected_session_revision: 0,
                        expected_goal_revision: 0,
                    },
                    admission_id: None,
                };
                let SubmitOutcomeV1::Accepted { admission, .. } =
                    state.submit(request, revision - 1).unwrap();
                pending_id = Some(admission.id);
            } else {
                state.revision = revision;
            }
            let bytes = serde_json::to_vec(&state).unwrap();
            let name = state_name(revision).unwrap();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(directory.join(&name))
                .unwrap();
            file.write_all(&bytes).unwrap();
            let commit = CommitV1 {
                schema_version: SCHEMA_VERSION,
                revision,
                state_name: name,
                content_hash: content_hash(&bytes),
                previous_commit_hash: None,
            };
            let mut line = serde_json::to_vec(&commit).unwrap();
            line.push(b'\n');
            assert!(line.len() <= MAX_COMMIT_RECORD_BYTES);
            journal_bytes.extend(line);
        }
        journal_bytes.extend_from_slice(b"unfinished-tail");
        fs::write(directory.join(JOURNAL), &journal_bytes).unwrap();
        fs::set_permissions(directory.join(JOURNAL), fs::Permissions::from_mode(0o600)).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let mut rescued = store.load().unwrap().unwrap();
        assert_eq!(rescued.revision, (MAX_SESSION_MANAGER_COMMITS - 1) as u64);
        assert_eq!(rescued.generation, 1);
        assert!(rescued.new_submit_closed);
        assert_eq!(fs::read(directory.join(JOURNAL)).unwrap(), journal_bytes);
        let pending_id = pending_id.unwrap();
        assert!(matches!(
            rescued.status(&pending_id).unwrap().state,
            AdmissionState::Reserved
        ));
        let mut stale = rescued.clone();
        stale.generation = 0;
        stale.new_submit_closed = false;
        let stale_pending = stale.status(&pending_id).unwrap().clone();
        stale
            .decide(
                &pending_id,
                stale.revision,
                stale_pending.revision,
                None,
                false,
                false,
            )
            .unwrap();
        assert!(matches!(
            store.persist(rescued.revision, &stale),
            Err(StoreError::CasConflict("session manager generation"))
        ));
        let pending = rescued.status(&pending_id).unwrap().clone();
        rescued
            .decide(
                &pending_id,
                rescued.revision,
                pending.revision,
                None,
                false,
                false,
            )
            .unwrap();
        store.persist(rescued.revision - 1, &rescued).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), rescued);
        assert_eq!(fs::read(directory.join(JOURNAL)).unwrap(), journal_bytes);
        let decided = rescued.status(&pending_id).unwrap().clone();
        let before = rescued.revision;
        let prepared = rescued
            .prepare_destination(&pending_id, before, decided.revision)
            .unwrap();
        store.persist(before, &rescued).unwrap();
        let before = rescued.revision;
        let appended = rescued
            .record_input_appended(&pending_id, before, prepared.revision, &current())
            .unwrap();
        store.persist(before, &rescued).unwrap();
        let before = rescued.revision;
        rescued
            .queue_run(&pending_id, before, appended.revision)
            .unwrap();
        store.persist(before, &rescued).unwrap();
        let before = rescued.revision;
        assert!(matches!(
            rescued.start_next(before).unwrap(),
            NextRunnableV1::Start { .. }
        ));
        store.persist(before, &rescued).unwrap();
        let running = rescued.status(&pending_id).unwrap().clone();
        let before = rescued.revision;
        rescued
            .terminal(
                &pending_id,
                before,
                running.revision,
                TerminalStatus::Completed,
            )
            .unwrap();
        store.persist(before, &rescued).unwrap();
        assert!(rescued.new_submit_closed);
        drop(store);
        let committed_len = journal_bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .unwrap()
            + 1;
        fs::write(directory.join(JOURNAL), &journal_bytes[..committed_len]).unwrap();
        let reopened = SessionManagerStore::open_owned(&path).unwrap();
        assert_eq!(reopened.load().unwrap().unwrap(), rescued);
        drop(reopened);
        fs::write(directory.join(JOURNAL), &journal_bytes).unwrap();
        let generation_journal = directory.join("generations/1").join(JOURNAL);
        let complete_generation = fs::read(&generation_journal).unwrap();
        let mut lines = complete_generation.split_inclusive(|byte| *byte == b'\n');
        lines.next().unwrap();
        fs::write(
            &generation_journal,
            lines.flatten().copied().collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(SessionManagerStore::open_owned(&path).is_err());
        fs::write(&generation_journal, complete_generation).unwrap();
        let mut lines = journal_bytes.split_inclusive(|byte| *byte == b'\n');
        lines.next().unwrap();
        fs::write(
            directory.join(JOURNAL),
            lines.flatten().copied().collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(SessionManagerStore::open_owned(&path).is_err());
        fs::write(directory.join(JOURNAL), &journal_bytes).unwrap();
        fs::remove_file(directory.join(state_name(0).unwrap())).unwrap();
        assert!(SessionManagerStore::open_owned(&path).is_err());
    }

    #[test]
    fn state_full_rescue_archives_terminal_evidence_and_rejects_missing_archive() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let directory = path.join(DIRECTORY);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut state = manager();
        let mut old_id = None;
        for number in 0..16 {
            let request = SubmitRequestV1 {
                parent_session: current(),
                payload: SubmissionPayloadV1 {
                    text: "x".repeat(32_000),
                    attachment_ids: Vec::new(),
                    route: DestinationMode::Current,
                    expected_session_revision: 0,
                    expected_goal_revision: 0,
                },
                admission_id: Some(
                    crate::session_manager::AdmissionId::new(format!("terminal-{number}")).unwrap(),
                ),
            };
            let SubmitOutcomeV1::Accepted { admission, .. } =
                state.submit(request, state.revision).unwrap();
            state
                .cancel(&admission.id, state.revision, admission.revision)
                .unwrap();
            old_id = Some(admission.id);
        }
        let pending_request = SubmitRequestV1 {
            parent_session: current(),
            payload: SubmissionPayloadV1 {
                text: "accepted".into(),
                attachment_ids: Vec::new(),
                route: DestinationMode::Current,
                expected_session_revision: 0,
                expected_goal_revision: 0,
            },
            admission_id: None,
        };
        let SubmitOutcomeV1::Accepted {
            admission: pending, ..
        } = state.submit(pending_request, state.revision).unwrap();
        let bytes = serde_json::to_vec(&state).unwrap();
        assert!(bytes.len() <= MAX_SESSION_MANAGER_STATE_BYTES);
        assert!(
            bytes.len() + state.remaining_capacity_reservation().1
                > MAX_SESSION_MANAGER_STATE_BYTES
        );
        let name = state_name(state.revision).unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(&name))
            .unwrap();
        file.write_all(&bytes).unwrap();
        let commit = CommitV1 {
            schema_version: SCHEMA_VERSION,
            revision: state.revision,
            state_name: name,
            content_hash: content_hash(&bytes),
            previous_commit_hash: None,
        };
        let mut journal = serde_json::to_vec(&commit).unwrap();
        journal.push(b'\n');
        fs::write(directory.join(JOURNAL), &journal).unwrap();
        fs::set_permissions(directory.join(JOURNAL), fs::Permissions::from_mode(0o600)).unwrap();
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let rescued = store.load().unwrap().unwrap();
        assert!(rescued.new_submit_closed);
        assert_eq!(
            rescued.status(&old_id.unwrap()).unwrap().payload.text.len(),
            32_000
        );
        assert!(matches!(
            rescued.status(&pending.id).unwrap().state,
            AdmissionState::Reserved
        ));
        drop(store);
        fs::remove_file(directory.join("generations/1/archive.json")).unwrap();
        assert!(SessionManagerStore::open_owned(&path).is_err());
        assert_eq!(fs::read(directory.join(JOURNAL)).unwrap(), journal);
    }

    #[test]
    fn interrupted_rescue_keeps_old_evidence_and_chooses_one_generation() {
        let root = root();
        let path = fs::canonicalize(root.path()).unwrap();
        let journal = full_legacy_fixture(&path);
        let directory = path.join(DIRECTORY);
        for point in 1..=8 {
            generation::fail_seal_once_at(point);
            assert!(
                SessionManagerStore::open_owned(&path).is_err(),
                "point {point}"
            );
            assert_eq!(fs::read(directory.join(JOURNAL)).unwrap(), journal);
            assert_eq!(directory.join("CURRENT").exists(), point == 8);
        }
        let store = SessionManagerStore::open_owned(&path).unwrap();
        let recovered = store.load().unwrap().unwrap();
        assert_eq!(recovered.generation, 1);
        assert!(recovered.new_submit_closed);
        assert_eq!(fs::read(directory.join(JOURNAL)).unwrap(), journal);
    }
}
